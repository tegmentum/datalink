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

/// Async, deep-reentrant + streaming `compose:dynlink` host (ports #221 deep
/// reentrancy, #223 CLI streaming, and the engine-as-provider path from the
/// reference sync host into the async crate). This is the surface #226
/// rewires sqlink's bespoke extension-loader onto.
pub mod reentrant;

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

/// Async-flavor bindings for instantiating a *provider* component. Identical
/// WIT/world to [`provider`], but bindgen generates `instantiate_async` and an
/// `async` `endpoint.handle` export call, so an async host (sqlink) can warm
/// and drive a resident provider without blocking. Mirrors sqlink's own
/// `dynlink_provider` bindgen (`imports`/`exports: { default: async }`). The
/// sync [`provider`] above is untouched; the resident-over-async backend uses
/// THIS module.
pub mod provider_async {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-provider",
        imports: { default: async },
        exports: { default: async },
    });
}

/// Async-flavor bindings for the guest-facing `compose:dynlink/linker`
/// import. Identical WIT and world to [`bindings`], but bindgen generates
/// `async fn` Host methods (`imports: { default: async }`) so an async host
/// (sqlink — tokio/hickory/reqwest) can satisfy the import without blocking.
/// Additive: the sync [`bindings`] above are untouched.
pub mod async_bindings {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-guest",
        imports: { default: async },
    });
}

pub use bindings::compose::dynlink::linker::Instance;
pub use bindings::sys::compose::types::{Error, ErrorCode};

/// The async-flavor opaque `instance` resource handle. A distinct generated
/// type from the sync [`Instance`] (it lives in [`async_bindings`]); the async
/// bridge/macro use this one.
pub use async_bindings::compose::dynlink::linker::Instance as AsyncInstance;

/// The async-flavor `Error`/`ErrorCode`. These are DISTINCT generated types
/// from the sync [`Error`]/[`ErrorCode`] above — each `bindgen!` of the same
/// WIT mints its own nominal Rust types — so the async trait, bridge, and the
/// async host methods must all speak THESE. The shape is identical; only the
/// nominal identity differs (which is what makes the sync flavor untouched).
pub use async_bindings::sys::compose::types::{Error as AsyncError, ErrorCode as AsyncErrorCode};

/// Build a host `Error` with the given code and message.
pub fn err(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        context: None,
    }
}

/// Build an async-flavor host [`AsyncError`] with the given code and message.
pub fn async_err(code: AsyncErrorCode, message: impl Into<String>) -> AsyncError {
    AsyncError {
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

/// Lower an async-flavor provider-side endpoint error (a distinct generated
/// Rust type with identical shape) into the guest-facing [`AsyncError`].
pub fn lower_provider_error_async(e: provider_async::sys::compose::types::Error) -> AsyncError {
    AsyncError {
        code: AsyncErrorCode::ExecTrap,
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
/// framework + ducklink hosts; this trait is correspondingly synchronous. A
/// fully-async host (sqlink) uses the additive async flavor below —
/// [`AsyncProviderBackend`] + [`AsyncDynLinkBridge`] +
/// [`impl_datalink_dynlink_async_host`] + [`add_to_linker_async`] — which also
/// supports keeping the `instance` resource table in the consumer's Store.
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
// ASYNC FLAVOR — additive. Mirrors the sync bridge for a fully-async host
// (sqlink). NONE of the sync types above change; ducklink (sync) is unaffected.
//
// Two things differ from the sync flavor, both demanded by sqlink's host:
//
//   1. The Host methods are `async fn` (sqlink's bindgen is `default: async`),
//      so the backend trait is async too. We use `async-trait` to get
//      boxed-`Send` futures — wasmtime's async host calls require `Send`
//      futures, and a generic backend whose `invoke` crosses `.await`s (CAS
//      lookups, a wasm instantiate, the SqliteRuntime shim) needs them boxed.
//
//   2. The `instance` resource table lives in the CONSUMER'S STORE, not in the
//      bridge. ducklink's sync bridge owns its table; sqlink keeps the table
//      on its per-Store state (`State.resources` / `RunState.resources`) so it
//      can share one table with WASI. The async bridge therefore takes the
//      table as a `&mut ResourceTable` PARAMETER on every routed call — the
//      macro threads it in from the store. The bridge owns only the backend.
// ===========================================================================

/// Async retype of a guest-facing `Resource<AsyncInstance>` to a host backing
/// type `Resource<H>`. Same rep, host-side compile-time tag only — see the
/// sync [`as_backing`].
fn as_backing_async<H: 'static>(r: &Resource<AsyncInstance>) -> Resource<H> {
    Resource::new_own(r.rep())
}

/// Async-flavor [`ProviderBackend`]. Same routing contract (resolve by
/// id/digest -> a cheap clonable `Handle`; `invoke`/`on_drop` over it), but
/// every method is `async`. This is where a consumer pushes its host-specific
/// policy: sqlink's trust gate, CAS-digest resolution, multi-tenant scoping,
/// fresh-store-per-invoke instantiation, and the built-in SqliteRuntime shim
/// all live in the consumer's `AsyncProviderBackend` impl(s) — the shared
/// bridge just routes to them.
///
/// `async-trait` boxes the returned futures as `Send` so they satisfy
/// wasmtime's async host-call requirement uniformly across backends.
#[async_trait::async_trait]
pub trait AsyncProviderBackend {
    /// What a resolved `instance` handle remembers so `invoke`/`on_drop` can
    /// reach the provider. Must be `Send + Sync + 'static` (it is parked in the
    /// store's resource table and read across `.await`).
    type Handle: Clone + Send + Sync + 'static;

    /// Resolve a provider by registry id (with any per-resolve work the
    /// backend's lifecycle needs), returning the handle to remember.
    async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, AsyncError>;

    /// Resolve a provider by content digest. Backends that don't support it
    /// return `AsyncErrorCode::NotImplemented`.
    async fn resolve_by_digest(&self, digest: &[u8]) -> Result<Self::Handle, AsyncError>;

    /// Forward an opaque message to the provider behind `handle`.
    async fn invoke(
        &self,
        handle: &Self::Handle,
        method: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, AsyncError>;

    /// Notification that a resolved handle was dropped by the guest. Default:
    /// no-op.
    async fn on_drop(&self, _handle: &Self::Handle) {}
}

/// The async per-component dynlink bridge: an [`AsyncProviderBackend`] only.
/// Unlike the sync [`DynLinkBridge`], it does NOT own the resource table — the
/// consumer keeps it on its Store and threads it in. The bridge holds the
/// backend and routes resolve/invoke/drop against a caller-supplied table.
///
/// `Clone` when the backend is (the backend is the only field; consumers'
/// backends are cheap `Arc`-shared clones), so a consumer that stores the
/// bridge on a `#[derive(Clone)]` state can clone it freely.
#[derive(Clone)]
pub struct AsyncDynLinkBridge<B: AsyncProviderBackend> {
    backend: B,
}

impl<B: AsyncProviderBackend + Sync> AsyncDynLinkBridge<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Access the underlying backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Resolve by id: ask the backend for a handle, then mint an opaque
    /// resource in the CALLER'S table.
    pub async fn resolve_by_id(
        &self,
        table: &mut ResourceTable,
        id: String,
    ) -> Result<Resource<AsyncInstance>, AsyncError> {
        let handle = self.backend.resolve_by_id(&id).await?;
        Self::push_handle(table, handle)
    }

    /// Resolve by digest: same, via the backend's digest path.
    pub async fn resolve_by_digest(
        &self,
        table: &mut ResourceTable,
        d: Vec<u8>,
    ) -> Result<Resource<AsyncInstance>, AsyncError> {
        let handle = self.backend.resolve_by_digest(&d).await?;
        Self::push_handle(table, handle)
    }

    fn push_handle(
        table: &mut ResourceTable,
        handle: B::Handle,
    ) -> Result<Resource<AsyncInstance>, AsyncError> {
        let backing = table
            .push(handle)
            .map_err(|e| async_err(AsyncErrorCode::InternalError, format!("table push: {e:?}")))?;
        Ok(Resource::new_own(backing.rep()))
    }

    /// Forward `method`/`payload` verbatim to the backend behind the handle
    /// (looked up in the caller's table).
    pub async fn invoke(
        &self,
        table: &mut ResourceTable,
        self_: Resource<AsyncInstance>,
        method: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, AsyncError> {
        let handle = table
            .get(&as_backing_async::<B::Handle>(&self_))
            .map_err(|e| {
                async_err(
                    AsyncErrorCode::InternalError,
                    format!("unknown dynlink handle: {e:?}"),
                )
            })?
            .clone();
        self.backend.invoke(&handle, &method, &payload).await
    }

    /// Release an `instance` handle (from the caller's table) and notify the
    /// backend.
    pub async fn drop_handle(
        &self,
        table: &mut ResourceTable,
        rep: Resource<AsyncInstance>,
    ) -> wasmtime::Result<()> {
        let handle = table.delete(as_backing_async::<B::Handle>(&rep))?;
        self.backend.on_drop(&handle).await;
        Ok(())
    }
}

/// Implement the async `compose:dynlink/linker` Host + HostInstance traits for
/// a store-state view type that can split itself, in ONE call, into BOTH a
/// `&AsyncDynLinkBridge<B>` (the routing + backend) AND a `&mut ResourceTable`
/// (the store-owned handle table). This is the seam that lets sqlink keep its
/// resource table in the Store while reusing the shared bridge.
///
/// The single-accessor `$split(&mut self) -> (&AsyncDynLinkBridge<B>, &mut
/// ResourceTable)` is what makes this borrow-check cleanly with no `unsafe`:
/// the CONSUMER produces the two non-aliasing references (typically: the
/// bridge is an `Arc`-shared field whose `&` doesn't conflict with a `&mut`
/// into a distinct table field). The bridge's routing methods then take the
/// table as a parameter.
///
/// Usage. A borrowed view names its lifetime up front (a `lifetime` fragment
/// is unambiguous, unlike a `tt`-repetition generics group):
/// ```ignore
/// // `$ty` exposes `fn split(&mut self) -> (&AsyncDynLinkBridge<B>, &mut ResourceTable)`.
/// impl_datalink_dynlink_async_host!(MyView, MyBackend, split);          // no generics
/// impl_datalink_dynlink_async_host!('a; MyView<'a>, MyBackend, split);  // borrowed view
/// ```
#[macro_export]
macro_rules! impl_datalink_dynlink_async_host {
    // Internal worker arm: matched first so the leading `@imp` token can't be
    // mis-consumed by the `$ty:ty` fragment (a `ty` fragment commits on its
    // first token and won't backtrack). `[$($gen:tt)*]` is empty or `<...>`.
    (@imp [$($gen:tt)*] $ty:ty, $backend:ty, $split:ident) => {
        impl $($gen)* $crate::async_bindings::sys::compose::types::Host for $ty {}

        // NOTE: wasmtime's `imports: { default: async }` generates the linker
        // Host/HostInstance traits with NATIVE `async fn` methods (not
        // async-trait boxing), so these impls use plain `async fn` — NOT
        // `#[async_trait]`. (The consumer's `AsyncProviderBackend` IS
        // async-trait, since it is a regular trait stored behind a generic.)
        impl $($gen)* $crate::async_bindings::compose::dynlink::linker::Host for $ty {
            async fn resolve_by_id(
                &mut self,
                id: ::std::string::String,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<$crate::AsyncInstance>,
                $crate::AsyncError,
            > {
                let (bridge, table) = self.$split();
                bridge.resolve_by_id(table, id).await
            }

            async fn resolve_by_digest(
                &mut self,
                d: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<$crate::AsyncInstance>,
                $crate::AsyncError,
            > {
                let (bridge, table) = self.$split();
                bridge.resolve_by_digest(table, d).await
            }
        }

        impl $($gen)* $crate::async_bindings::compose::dynlink::linker::HostInstance for $ty {
            async fn invoke(
                &mut self,
                self_: ::wasmtime::component::Resource<$crate::AsyncInstance>,
                method: ::std::string::String,
                payload: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::AsyncError> {
                let (bridge, table) = self.$split();
                bridge.invoke(table, self_, method, payload).await
            }

            async fn drop(
                &mut self,
                rep: ::wasmtime::component::Resource<$crate::AsyncInstance>,
            ) -> ::wasmtime::Result<()> {
                let (bridge, table) = self.$split();
                bridge.drop_handle(table, rep).await
            }
        }
    };
    // Entry: a borrowed view, with its lifetime named up front as a
    // `lifetime` fragment (unambiguous — no `tt`-repetition local ambiguity).
    // Matched before the no-generics arm.
    ($life:lifetime; $ty:ty, $backend:ty, $split:ident) => {
        $crate::impl_datalink_dynlink_async_host!(@imp [<$life>] $ty, $backend, $split);
    };
    // Entry: no generics on the view type.
    ($ty:ty, $backend:ty, $split:ident) => {
        $crate::impl_datalink_dynlink_async_host!(@imp [] $ty, $backend, $split);
    };
}

/// Re-export `async-trait` so the [`impl_datalink_dynlink_async_host`] macro
/// can reference it without the consumer adding the dependency.
#[doc(hidden)]
pub mod async_trait_reexport {
    pub use async_trait::async_trait;
}

/// Add the async `compose:dynlink/linker` host import to a guest linker over
/// any store type `T` that implements the async linker Host traits (via
/// [`impl_datalink_dynlink_async_host`]). WASI must be added separately.
pub fn add_to_linker_async<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: async_bindings::compose::dynlink::linker::Host
        + async_bindings::compose::dynlink::linker::HostInstance
        + async_bindings::sys::compose::types::Host
        + Send
        + 'static,
{
    async_bindings::DynlinkGuest::add_to_linker::<_, HasSelf<T>>(linker, |s| s).map_err(|e| {
        wasmtime::Error::msg(format!("add async compose:dynlink/linker to linker: {e:?}"))
    })
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

/// Substitute for `wasmtime::component::Linker::define_unknown_imports_as_traps`
/// that pre-filters top-level imports already satisfied by a prior
/// `add_to_linker_*` call.
///
/// Wasmtime-46's stock impl (`wasmtime-46.0.1/src/runtime/component/linker.rs`
/// line 387) unconditionally invokes `linker.instance(item_name)?` for every
/// top-level `ComponentInstance` import. That is intentional for a linker with
/// no prior `add_to_linker`, but it collides with the `wasi:*/*@0.2.x`
/// instances that `wasmtime_wasi::p2::add_to_linker_{sync,async}` just
/// registered:
///
/// ```text
/// provider '<id>': define_unknown_imports_as_traps:
///   map entry `wasi:cli/environment@0.2.12` defined twice
/// ```
///
/// The stock impl's "skip if already defined" guard (line 362) only fires for
/// `ComponentFunc` imports; the `ComponentInstance` arm has no such check. We
/// mirror the stock impl's tree-walk but skip any top-level import whose name
/// lives in the `wasi:` namespace — those belong to the WASI p2 linker that
/// already populated them (semver-matched under the covers).
///
/// Non-wasi dangling imports (the partial backend's raison d'être — e.g.
/// `postgis:sfcgal-provider/...` on a core-only backend) still trap-stub
/// through the recursion, exactly as before.
fn define_missing_imports_as_traps<T: 'static>(
    linker: &mut Linker<T>,
    component: &Component,
    engine: &Engine,
) -> wasmtime::Result<()> {
    let ct = component.component_type();
    let mut root = linker.root();
    for (import_name, ext) in ct.imports(engine) {
        if import_name.starts_with("wasi:") {
            // Already wired by `wasmtime_wasi::p2::add_to_linker_{sync,async}`
            // above. Defining it here would either error ("defined twice") or
            // (under shadowing) wipe out the real host impls with trap stubs.
            continue;
        }
        stub_component_item(&mut root, import_name, &ext.ty, engine, None)?;
    }
    Ok(())
}

/// Recursively trap-stub a single component import, mirroring the tree walk
/// in wasmtime's stock `define_unknown_imports_as_traps` (linker.rs lines
/// 354-410). See [`define_missing_imports_as_traps`] for why we reimplement
/// this instead of calling the wasmtime helper.
fn stub_component_item<T: 'static>(
    linker: &mut wasmtime::component::LinkerInstance<'_, T>,
    name: &str,
    item: &wasmtime::component::types::ComponentItem,
    engine: &Engine,
    parent_instance: Option<&str>,
) -> wasmtime::Result<()> {
    use wasmtime::component::types::ComponentItem;
    use wasmtime::component::ResourceType;
    match item {
        ComponentItem::ComponentFunc(_) => {
            let fq = match parent_instance {
                Some(p) => format!("{p}#{name}"),
                None => name.to_string(),
            };
            linker.func_new(name, move |_, _, _, _| {
                Err(wasmtime::Error::msg(format!(
                    "unknown import: `{fq}` has not been defined"
                )))
            })?;
        }
        ComponentItem::ComponentInstance(ci) => {
            let mut inner = linker.instance(name)?;
            for (export_name, export) in ci.exports(engine) {
                stub_component_item(&mut inner, export_name, &export.ty, engine, Some(name))?;
            }
        }
        ComponentItem::Resource(_) => {
            let ty = ResourceType::host::<()>();
            linker.resource(name, ty, |_, _| Ok(()))?;
        }
        // Type / CoreFunc / Module / Component are not stubbable via the
        // component-linker trap surface; the stock helper bails on Component /
        // Module and drops the rest silently. Matches that behavior.
        _ => {}
    }
    Ok(())
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
        // Partial backends (postgis-core-provider, postgis-sfcgal-provider,
        // etc. under datafission's Phase 3 backend split) carry dangling
        // non-wasi imports for the deps that the sibling sub-ext backends
        // will satisfy. wasmtime's stock instantiate refuses; wire trap-
        // on-call stubs so instantiation succeeds and the trap only fires
        // if the dispatcher actually routes a call into a dangling import.
        // Full-surface providers (postgis-composed-provider et al.) have
        // no dangling imports and this call is a no-op.
        define_missing_imports_as_traps(&mut linker, &entry.component, engine).map_err(|e| {
            err(
                ErrorCode::EmitLinkError,
                format!("provider '{id}': define_missing_imports_as_traps: {e}"),
            )
        })?;
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

// ===========================================================================
// AsyncResidentBackend — the async-flavor analog of ResidentBackend.
//
// Instantiate a registered `dynlink-provider` component ONCE (lazily, on first
// resolve) into a single resident store, and reuse it across every
// resolve/invoke — "one heavy provider serving many guests" — but over the
// ASYNC bridge ([`AsyncProviderBackend`]). This is the enabler for resident
// S3/HTTP providers on a fully-async host (sqlink): warm the provider once,
// then route every host S3/HTTP call through it at native-like throughput.
//
// Why a SEPARATE registry from the sync [`ProviderRegistry`]: the async store
// is driven by `instantiate_async` / an async `call_handle`, which cross
// `.await` points. The single resident Store therefore lives behind a
// `tokio::sync::Mutex` (held across those awaits to serialize the one Store),
// and instantiation uses the async [`provider_async`] bindings. The sync
// registry's `std::sync::Mutex<Store>` could not be held across an await.
// ===========================================================================

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use tokio::sync::Mutex as AsyncMutex;

/// A resident, async-instantiated provider. Materialized ONCE and shared across
/// every resolve/invoke. Holds its own store so calls never touch a guest's
/// store. Guarded by an [`AsyncMutex`] (in [`AsyncSlot`]) so the single Store is
/// serialized across the async `instantiate`/`call_handle` awaits.
struct AsyncResidentProvider {
    store: Store<ProviderState>,
    instance: provider_async::DynlinkProvider,
}

/// The lazily-materialized state for one provider id: the compiled component +
/// preopens needed to instantiate, plus the resident instance once warmed. The
/// whole slot sits behind an [`AsyncMutex`] so materialization and every invoke
/// (which mutably drive the one Store) are serialized without blocking the
/// async runtime.
struct AsyncSlot {
    engine: Engine,
    component: Component,
    path: PathBuf,
    preopens: Vec<ProviderPreopen>,
    /// Whether the provider's OWN store is granted outbound network egress
    /// (TCP + IP name lookup) on materialization. A provider that reaches the
    /// network from inside wasm — e.g. the `s3-endpoint` provider, which signs
    /// and sends S3 requests over `wasi:sockets` + rustls — needs this; a
    /// pure-compute provider (pylon, echo) does not. Off by default, so the
    /// host opts a provider into egress explicitly (the network grant the host
    /// supplies to the provider store; the host's policy gate stays upstream of
    /// the invoke, not here).
    network: bool,
    /// `Some(..)` once instantiated; reused across all subsequent
    /// resolves/invokes (the warm-once shared model).
    resident: Option<AsyncResidentProvider>,
}

/// Per-id registration record for the async registry. `slot` carries the
/// materialization inputs + the warmed instance behind an async mutex;
/// `materialized` + `handle_count` are lock-free counters so test/inspection
/// helpers never have to take the (possibly in-use) slot lock.
struct AsyncProviderEntry {
    slot: Arc<AsyncMutex<AsyncSlot>>,
    materialized: Arc<AtomicBool>,
    handle_count: Arc<AtomicU64>,
}

struct AsyncRegistryInner {
    engine: Engine,
    providers: HashMap<String, AsyncProviderEntry>,
    digest_to_id: HashMap<Vec<u8>, String>,
}

/// The async provider registry: the wasm engine plus an `id -> AsyncProviderEntry`
/// map. The engine drives async instantiation/calls (the wasmtime 46 component
/// model is async-capable by default). Cloneable (`Arc`-shared) so it can be both
/// the registration surface AND the [`AsyncResidentBackend`]'s shared state.
#[derive(Clone)]
pub struct AsyncProviderRegistry {
    inner: Arc<Mutex<AsyncRegistryInner>>,
}

impl AsyncProviderRegistry {
    /// Create a registry over a component-model engine (build it with
    /// `Config::wasm_component_model(true)`).
    pub fn new(engine: Engine) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AsyncRegistryInner {
                engine,
                providers: HashMap::new(),
                digest_to_id: HashMap::new(),
            })),
        }
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, compiling
    /// it now (async instantiation is deferred to first resolve).
    pub fn register_provider(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(), String> {
        self.register_provider_with_preopens(id, path, Vec::new())
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, with
    /// directories preopened into its OWN store on materialization (no network
    /// grant).
    pub fn register_provider_with_preopens(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
    ) -> Result<(), String> {
        self.register_provider_with_options(id, path, preopens, false)
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, granting
    /// its OWN store outbound network egress (TCP + IP name lookup) on
    /// materialization. This is the registration the resident `s3-endpoint` /
    /// HTTP providers use: they reach the network from inside wasm
    /// (`wasi:sockets` + rustls), so the host supplies the egress capability to
    /// the provider store here. The host's capability/policy gate stays UPSTREAM
    /// of the invoke (it is not moved into the provider).
    pub fn register_provider_with_network(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(), String> {
        self.register_provider_with_options(id, path, Vec::new(), true)
    }

    /// Register a `dynlink-provider`-world wasm component under `id`, with
    /// directories preopened into its OWN store and an explicit `network` egress
    /// grant. `register_provider_with_preopens` / `register_provider_with_network`
    /// are the common-case wrappers.
    pub fn register_provider_with_options(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
        network: bool,
    ) -> Result<(), String> {
        let id = id.into();
        let path = path.into();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let component = Component::from_binary(&inner.engine, &bytes)
            .map_err(|e| format!("compile provider {}: {e}", path.display()))?;
        let engine = inner.engine.clone();
        inner.providers.insert(
            id,
            AsyncProviderEntry {
                slot: Arc::new(AsyncMutex::new(AsyncSlot {
                    engine,
                    component,
                    path,
                    preopens,
                    network,
                    resident: None,
                })),
                materialized: Arc::new(AtomicBool::new(false)),
                handle_count: Arc::new(AtomicU64::new(0)),
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
    /// Lock-free — reads the `materialized` flag, never the slot — so it is safe
    /// to call while the provider is mid-invoke. Used to assert ONE instance
    /// backs N resolves.
    pub fn resident_count(&self, id: &str) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .providers
            .get(id)
            .map(|e| usize::from(e.materialized.load(AtomicOrdering::SeqCst)))
            .unwrap_or(0)
    }

    /// How many live `instance` handles point at `id`'s resident provider.
    pub fn handle_count(&self, id: &str) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .providers
            .get(id)
            .map(|e| e.handle_count.load(AtomicOrdering::SeqCst))
            .unwrap_or(0)
    }

    /// Look up the per-id slot + counters, cloning the cheap `Arc`s out from
    /// under the (synchronous) registry lock so the caller can `.await` on the
    /// slot WITHOUT holding the std mutex across the await.
    fn entry_handles(
        &self,
        id: &str,
    ) -> Option<(Arc<AsyncMutex<AsyncSlot>>, Arc<AtomicBool>, Arc<AtomicU64>)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .providers
            .get(id)
            .map(|e| (e.slot.clone(), e.materialized.clone(), e.handle_count.clone()))
    }

    fn digest_id(&self, digest: &[u8]) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.digest_to_id.get(digest).cloned()
    }
}

/// The opaque handle an [`AsyncResidentBackend`] remembers for each resolved
/// `instance`. It does NOT own the provider — the resident provider lives in the
/// shared slot. The handle remembers WHICH id it resolved plus cheap `Arc`s to
/// that id's slot + handle counter, so `invoke`/`on_drop` never re-take the
/// registry lock.
#[derive(Clone)]
pub struct AsyncResidentHandle {
    id: String,
    slot: Arc<AsyncMutex<AsyncSlot>>,
    handle_count: Arc<AtomicU64>,
}

/// The async resident provider lifecycle: instantiate a registered provider
/// ONCE (lazily, on first resolve) into a single resident store, and reuse it
/// across every resolve/invoke — the async analog of [`ResidentBackend`], for a
/// fully-async host (sqlink). This is the backend that makes resident S3/HTTP
/// component providers possible: warm once, route every host I/O call through
/// the same instance. Wraps an [`AsyncProviderRegistry`].
#[derive(Clone)]
pub struct AsyncResidentBackend {
    registry: AsyncProviderRegistry,
}

impl AsyncResidentBackend {
    pub fn new(registry: AsyncProviderRegistry) -> Self {
        Self { registry }
    }

    /// Access the shared registry (e.g. to inspect `resident_count`).
    pub fn registry(&self) -> &AsyncProviderRegistry {
        &self.registry
    }
}

#[async_trait::async_trait]
impl AsyncProviderBackend for AsyncResidentBackend {
    type Handle = AsyncResidentHandle;

    async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, AsyncError> {
        let (slot, materialized, handle_count) = self.registry.entry_handles(id).ok_or_else(|| {
            async_err(AsyncErrorCode::InvalidInput, format!("unknown provider id: {id}"))
        })?;
        // Materialize ONCE under the slot's async lock (held across the
        // instantiate await — the std registry lock was already released).
        {
            let mut guard = slot.lock().await;
            if guard.resident.is_none() {
                materialize_resident_async(&mut guard, id).await?;
                materialized.store(true, AtomicOrdering::SeqCst);
                eprintln!(
                    "[compose-dynlink] async resident provider '{}' instantiated ONCE from {} (shared across resolves)",
                    id,
                    guard.path.display()
                );
            } else {
                eprintln!(
                    "[compose-dynlink] async resolve '{id}' reuses the existing resident provider (1 instance)"
                );
            }
        }
        handle_count.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(AsyncResidentHandle {
            id: id.to_string(),
            slot,
            handle_count,
        })
    }

    async fn resolve_by_digest(&self, d: &[u8]) -> Result<Self::Handle, AsyncError> {
        match self.registry.digest_id(d) {
            Some(id) => self.resolve_by_id(&id).await,
            None => Err(async_err(
                AsyncErrorCode::NotImplemented,
                "resolve-by-digest: no digest->id mapping registered (use register_digest)",
            )),
        }
    }

    async fn invoke(
        &self,
        handle: &Self::Handle,
        method: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, AsyncError> {
        let mut guard = handle.slot.lock().await;
        let resident = guard.resident.as_mut().ok_or_else(|| {
            async_err(
                AsyncErrorCode::InternalError,
                format!("provider '{}' not resident", handle.id),
            )
        })?;
        // Split the &mut borrow across the two fields so the endpoint accessor
        // (borrows `instance`) and the call (borrows `store`) don't conflict.
        let AsyncResidentProvider { store, instance } = resident;
        let result = instance
            .compose_dynlink_endpoint()
            .call_handle(&mut *store, method, payload)
            .await
            .map_err(|e| {
                async_err(
                    AsyncErrorCode::ExecTrap,
                    format!("provider '{}' handle trapped: {e:?}", handle.id),
                )
            })?;
        result.map_err(lower_provider_error_async)
    }

    async fn on_drop(&self, handle: &Self::Handle) {
        let prev = handle.handle_count.load(AtomicOrdering::SeqCst);
        if prev > 0 {
            handle.handle_count.fetch_sub(1, AtomicOrdering::SeqCst);
        }
    }
}

/// Materialize (instantiate ONCE via the async bindings) the resident provider
/// inside an already-locked slot.
async fn materialize_resident_async(slot: &mut AsyncSlot, id: &str) -> Result<(), AsyncError> {
    let mut linker: Linker<ProviderState> = Linker::new(&slot.engine);
    // WASI host impls. A network-granted provider does real socket I/O from
    // inside wasm (wasi:sockets), whose host ops suspend on wasi:io/poll — so it
    // needs the ASYNC WASI linker: the socket futures are awaited through the
    // component call (call_handle().await) with no nested `block_on`. (The sync
    // WASI linker's socket ops call `block_on` internally, which panics when the
    // provider is driven from an async runtime.) A pure-compute provider
    // (pylon, echo) keeps the sync WASI linker — it never suspends.
    if slot.network {
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| {
            async_err(AsyncErrorCode::EmitLinkError, format!("provider async wasi linker: {e}"))
        })?;
    } else {
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| {
            async_err(AsyncErrorCode::EmitLinkError, format!("provider wasi linker: {e}"))
        })?;
    }
    // Trap-stubs for any dangling non-wasi import on partial backends
    // — see the sync analog in `materialize_resident` for the full
    // rationale. Full-surface providers have no dangling imports and
    // this call is a no-op.
    define_missing_imports_as_traps(&mut linker, &slot.component, &slot.engine).map_err(|e| {
        async_err(
            AsyncErrorCode::EmitLinkError,
            format!("provider '{id}': define_missing_imports_as_traps: {e}"),
        )
    })?;
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio();
    if slot.network {
        // The host supplies outbound network egress to THIS provider's store
        // (the s3-endpoint / HTTP providers sign+send over wasi:sockets+rustls).
        builder
            .inherit_network()
            .allow_ip_name_lookup(true)
            .allow_tcp(true);
    }
    for po in &slot.preopens {
        builder
            .preopened_dir(&po.host, &po.guest, DirPerms::all(), FilePerms::all())
            .map_err(|e| {
                async_err(
                    AsyncErrorCode::InvalidInput,
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
    let mut store = Store::new(&slot.engine, state);
    let instance =
        provider_async::DynlinkProvider::instantiate_async(&mut store, &slot.component, &linker)
            .await
            .map_err(|e| {
                async_err(AsyncErrorCode::ExecTrap, format!("instantiate provider '{id}': {e:?}"))
            })?;
    slot.resident = Some(AsyncResidentProvider { store, instance });
    Ok(())
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

    // --- async flavor ---

    /// In-process async backend mirroring `EchoBackend`, exercising the
    /// `AsyncDynLinkBridge` routing with a CONSUMER-OWNED resource table (the
    /// store-resource-table seam sqlink relies on). No wasm involved.
    #[derive(Clone, Default)]
    struct AsyncEchoBackend {
        live: Arc<AtomicU64>,
    }

    #[derive(Clone)]
    struct AsyncEchoHandle {
        id: String,
        live: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl AsyncProviderBackend for AsyncEchoBackend {
        type Handle = AsyncEchoHandle;

        async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, AsyncError> {
            self.live.fetch_add(1, Ordering::SeqCst);
            Ok(AsyncEchoHandle {
                id: id.to_string(),
                live: self.live.clone(),
            })
        }

        async fn resolve_by_digest(&self, _d: &[u8]) -> Result<Self::Handle, AsyncError> {
            Err(async_err(AsyncErrorCode::NotImplemented, "no digest map"))
        }

        async fn invoke(
            &self,
            handle: &Self::Handle,
            method: &str,
            payload: &[u8],
        ) -> Result<Vec<u8>, AsyncError> {
            match method {
                "upper" => Ok(String::from_utf8_lossy(payload).to_uppercase().into_bytes()),
                "id" => Ok(handle.id.clone().into_bytes()),
                other => Err(async_err(
                    AsyncErrorCode::InvalidInput,
                    format!("unknown method {other}"),
                )),
            }
        }

        async fn on_drop(&self, handle: &Self::Handle) {
            handle.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn async_bridge_routes_resolve_invoke_drop_with_store_table() {
        let backend = AsyncEchoBackend::default();
        let live = backend.live.clone();
        let bridge = AsyncDynLinkBridge::new(backend);
        // The table lives "in the store" (here just a local), threaded into
        // every routed call — exactly sqlink's seam.
        let mut table = ResourceTable::new();

        let handle = bridge
            .resolve_by_id(&mut table, "prov".to_string())
            .await
            .expect("resolve");
        assert_eq!(live.load(Ordering::SeqCst), 1, "one live handle after resolve");

        let upper = bridge
            .invoke(
                &mut table,
                Resource::new_own(handle.rep()),
                "upper".to_string(),
                b"hello from async dlopen".to_vec(),
            )
            .await
            .expect("invoke upper");
        assert_eq!(upper, b"HELLO FROM ASYNC DLOPEN");

        let id = bridge
            .invoke(
                &mut table,
                Resource::new_own(handle.rep()),
                "id".to_string(),
                Vec::new(),
            )
            .await
            .expect("invoke id");
        assert_eq!(id, b"prov");

        bridge
            .drop_handle(&mut table, handle)
            .await
            .expect("drop");
        assert_eq!(live.load(Ordering::SeqCst), 0, "handle released on drop");
    }

    #[tokio::test]
    async fn async_bridge_digest_unmapped_is_not_implemented() {
        let bridge = AsyncDynLinkBridge::new(AsyncEchoBackend::default());
        let mut table = ResourceTable::new();
        match bridge.resolve_by_digest(&mut table, vec![0u8; 32]).await {
            Err(e) => assert!(matches!(e.code, AsyncErrorCode::NotImplemented)),
            Ok(_) => panic!("unmapped digest must not resolve"),
        }
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

    #[tokio::test]
    async fn async_resident_backend_reports_digest_unmapped() {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        let registry = AsyncProviderRegistry::new(engine);
        let backend = AsyncResidentBackend::new(registry);
        match backend.resolve_by_digest(&[0u8; 32]).await {
            Err(e) => assert!(matches!(e.code, AsyncErrorCode::NotImplemented)),
            Ok(_) => panic!("unmapped digest must not resolve"),
        }
    }

    /// Path to a per-sub-ext partial-backend provider (Phase 3 backend
    /// split, sqlink-lib #823). The postgis-core-provider wraps
    /// postgis-core-composed.wasm — a 33 MB partial backend that carries
    /// 17 dangling non-wasi imports for raster / sfcgal / format-encoder
    /// deps not included in the core plan. If materialization succeeds,
    /// `define_unknown_imports_as_traps` is doing its job.
    fn postgis_core_provider_wasm() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join("git/postgis-wasm/build/plans/postgis-core-provider-composed.wasm")
    }

    /// Trap-stubs substrate proof (#823 Phase 3 Commit 3): a partial
    /// backend with 17 dangling non-wasi imports materializes cleanly,
    /// and the dispatcher's built-in "unknown method" fallthrough
    /// answers arms outside the partial's coverage — no trap needed
    /// because those methods never reach a dangling import in the
    /// first place. The trap-on-call stubs are load-bearing at
    /// instantiate time; they earn their keep only if some code path
    /// inside the partial backend organically wanders into a dangling
    /// interface (e.g. postgis-composed internally calling gdal for
    /// a raster arm the dispatcher shouldn't have routed to). We
    /// don't fabricate such a call here — the substrate win is that
    /// instantiate NO LONGER refuses.
    #[test]
    fn resident_materializes_partial_backend_via_trap_stubs() {
        let provider = postgis_core_provider_wasm();
        if !provider.exists() {
            eprintln!(
                "skipping resident_materializes_partial_backend_via_trap_stubs: prebuilt provider not found ({})",
                provider.display()
            );
            return;
        }

        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");

        let registry = ProviderRegistry::new(engine);
        registry
            .register_provider("postgis-core", &provider)
            .expect("register postgis-core provider");
        assert_eq!(registry.resident_count("postgis-core"), 0, "lazy pre-resolve");

        let backend = ResidentBackend::new(registry.clone());
        // The load-bearing assertion: before the trap-stub fix this
        // materialization failed with "unknown import <one of the 17
        // dangling non-wasi imports> has not been defined".
        let _handle = backend
            .resolve_by_id("postgis-core")
            .expect("materialize partial backend: define_unknown_imports_as_traps must satisfy dangling non-wasi imports");
        assert_eq!(
            registry.resident_count("postgis-core"),
            1,
            "partial backend now resident: trap-stubs unblocked instantiation"
        );
    }

    /// Path to the framework's prebuilt `dynlink_echo_provider.wasm` (a real
    /// `compose:dynlink/endpoint` provider). The async-resident warm-once test
    /// skips gracefully if it isn't built (mirrors ducklink's sync dlopen test).
    fn echo_provider_wasm() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(
            "git/webassembly-component-orchestration/examples/dynlink-echo-provider/target/wasm32-wasip2/release/dynlink_echo_provider.wasm",
        )
    }

    /// The headline Part-1 proof: an in-process ASYNC resident backend warms a
    /// real wasm provider ONCE and reuses it across multiple resolves/invokes —
    /// the async analog of ducklink's sync resident proof, and the property the
    /// resident S3/HTTP migration depends on.
    #[tokio::test]
    async fn async_resident_warm_once_reuse() {
        let provider = echo_provider_wasm();
        if !provider.exists() {
            eprintln!(
                "skipping async_resident_warm_once_reuse: prebuilt provider not found ({})",
                provider.display()
            );
            return;
        }

        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");

        let registry = AsyncProviderRegistry::new(engine);
        registry
            .register_provider("echo", &provider)
            .expect("register echo provider");
        // Not materialized until the first resolve.
        assert_eq!(registry.resident_count("echo"), 0, "lazy: no instance pre-resolve");

        let backend = AsyncResidentBackend::new(registry.clone());
        let bridge = AsyncDynLinkBridge::new(backend);
        // The handle table lives "in the store" (here a local), threaded into
        // every routed call — exactly sqlink's seam.
        let mut table = ResourceTable::new();

        // resolve #1 -> materializes the resident provider ONCE.
        let h1 = bridge
            .resolve_by_id(&mut table, "echo".to_string())
            .await
            .expect("resolve #1");
        assert_eq!(registry.resident_count("echo"), 1, "one resident after first resolve");
        assert_eq!(registry.handle_count("echo"), 1, "one live handle");

        let up = bridge
            .invoke(
                &mut table,
                Resource::new_own(h1.rep()),
                "upper".to_string(),
                b"hello from async dlopen".to_vec(),
            )
            .await
            .expect("invoke upper #1");
        assert_eq!(up, b"HELLO FROM ASYNC DLOPEN");

        // resolve #2 -> REUSES the same resident instance (warm-once shared).
        let h2 = bridge
            .resolve_by_id(&mut table, "echo".to_string())
            .await
            .expect("resolve #2");
        assert_eq!(
            registry.resident_count("echo"),
            1,
            "warm-once: STILL one resident across two resolves"
        );
        assert_eq!(registry.handle_count("echo"), 2, "two live handles, one instance");

        let echoed = bridge
            .invoke(
                &mut table,
                Resource::new_own(h2.rep()),
                "echo".to_string(),
                b"abc".to_vec(),
            )
            .await
            .expect("invoke echo #2");
        assert_eq!(echoed, b"abc", "second handle drives the SAME resident instance");

        // Drop both handles -> count returns to zero, instance stays resident.
        bridge.drop_handle(&mut table, h1).await.expect("drop h1");
        bridge.drop_handle(&mut table, h2).await.expect("drop h2");
        assert_eq!(registry.handle_count("echo"), 0, "handles released on drop");
        assert_eq!(registry.resident_count("echo"), 1, "resident instance persists after drops");
    }

    /// Path to the in-repo `s3-endpoint` provider wasm (built by
    /// `components/s3-endpoint/build.sh`). The resident-S3 test skips gracefully
    /// if it isn't built yet.
    fn s3_endpoint_wasm() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../components/s3-endpoint/target/wasm32-wasip2/release/s3_endpoint.wasm")
    }

    /// Encode a CBOR map of `(key, value)` text/value pairs.
    fn cbor_map(pairs: Vec<(&str, ciborium::value::Value)>) -> Vec<u8> {
        use ciborium::value::Value;
        let v = Value::Map(
            pairs
                .into_iter()
                .map(|(k, val)| (Value::Text(k.to_string()), val))
                .collect(),
        );
        let mut out = Vec::new();
        ciborium::ser::into_writer(&v, &mut out).unwrap();
        out
    }

    /// The resident-S3 proof: register the REAL `s3-endpoint` provider with a
    /// network grant, warm it ONCE through the `AsyncResidentBackend`, and drive
    /// its `sign` op (offline, deterministic) across multiple resolves —
    /// asserting one resident instance backs N calls and that the SigV4 output
    /// matches AWS's published "GET Object" vector. This is the end-to-end
    /// enabler proof for routing a host's native S3 path through a resident
    /// provider.
    #[tokio::test]
    async fn async_resident_s3_endpoint_sign_warm_once() {
        use ciborium::value::Value;

        let provider = s3_endpoint_wasm();
        if !provider.exists() {
            eprintln!(
                "skipping async_resident_s3_endpoint_sign_warm_once: provider not built ({})",
                provider.display()
            );
            return;
        }

        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        // Network providers use the async WASI linker (awaited socket futures).
        // Note: `Config::async_support` was deprecated in wasmtime 47 (no-op);
        // the async wiring is now inferred from the linker/host functions.
        let engine = Engine::new(&config).expect("engine");

        let registry = AsyncProviderRegistry::new(engine);
        // Network-granted registration — the egress wiring a host supplies to
        // the resident S3 provider's store.
        registry
            .register_provider_with_network("s3", &provider)
            .expect("register s3-endpoint provider");
        assert_eq!(registry.resident_count("s3"), 0, "lazy: not materialized pre-resolve");

        let backend = AsyncResidentBackend::new(registry.clone());
        let bridge = AsyncDynLinkBridge::new(backend);
        let mut table = ResourceTable::new();

        // resolve #1 -> warms the resident provider ONCE.
        let h1 = bridge
            .resolve_by_id(&mut table, "s3".to_string())
            .await
            .expect("resolve #1");
        assert_eq!(registry.resident_count("s3"), 1, "one resident after first resolve");

        // The AWS doc "GET Object" SigV4 vector, driven through the resident
        // provider's `sign` (dry-run) op over the CBOR envelope.
        let endpoint = Value::Map(vec![
            (Value::Text("url".into()), Value::Text("https://s3.amazonaws.com".into())),
            (Value::Text("region".into()), Value::Text("us-east-1".into())),
            (Value::Text("path_style".into()), Value::Bool(false)),
        ]);
        let creds = Value::Map(vec![
            (Value::Text("access_key_id".into()), Value::Text("AKIAIOSFODNN7EXAMPLE".into())),
            (
                Value::Text("secret_access_key".into()),
                Value::Text("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()),
            ),
            (Value::Text("session_token".into()), Value::Null),
        ]);
        let sign_req = cbor_map(vec![
            ("method", Value::Text("GET".into())),
            ("endpoint", endpoint),
            ("credentials", creds),
            ("bucket", Value::Text("examplebucket".into())),
            ("key", Value::Text("test.txt".into())),
            (
                "extra_headers",
                Value::Array(vec![Value::Array(vec![
                    Value::Text("range".into()),
                    Value::Text("bytes=0-9".into()),
                ])]),
            ),
            ("amz_date", Value::Text("20130524T000000Z".into())),
        ]);

        let resp_bytes = bridge
            .invoke(
                &mut table,
                Resource::new_own(h1.rep()),
                "sign".to_string(),
                sign_req.clone(),
            )
            .await
            .expect("invoke sign #1");
        let resp: Value = ciborium::de::from_reader(&*resp_bytes).unwrap();
        let field = |v: &Value, k: &str| -> Option<String> {
            if let Value::Map(m) = v {
                for (key, val) in m {
                    if matches!(key, Value::Text(s) if s == k) {
                        if let Value::Text(s) = val {
                            return Some(s.clone());
                        }
                    }
                }
            }
            None
        };
        let authz = field(&resp, "authorization").expect("authorization in sign response");
        assert!(
            authz.contains(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "resident s3-endpoint SigV4 vector mismatch: {authz}"
        );
        assert_eq!(
            field(&resp, "url").as_deref(),
            Some("https://examplebucket.s3.amazonaws.com/test.txt")
        );

        // resolve #2 -> REUSES the same resident instance (warm-once shared).
        let h2 = bridge
            .resolve_by_id(&mut table, "s3".to_string())
            .await
            .expect("resolve #2");
        assert_eq!(
            registry.resident_count("s3"),
            1,
            "warm-once: STILL one resident across two resolves"
        );
        assert_eq!(registry.handle_count("s3"), 2, "two handles, one instance");

        // A second op through the second handle hits the SAME resident store.
        let manifest = bridge
            .invoke(
                &mut table,
                Resource::new_own(h2.rep()),
                "manifest".to_string(),
                Vec::new(),
            )
            .await
            .expect("invoke manifest #2");
        let mv: Value = ciborium::de::from_reader(&*manifest).unwrap();
        assert_eq!(field(&mv, "name").as_deref(), Some("s3-endpoint"));

        bridge.drop_handle(&mut table, h1).await.expect("drop h1");
        bridge.drop_handle(&mut table, h2).await.expect("drop h2");
        assert_eq!(registry.handle_count("s3"), 0, "handles released");
        assert_eq!(registry.resident_count("s3"), 1, "resident persists after drops");
    }
}
