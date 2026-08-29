//! `datalink-dynlink` — shared host machinery for the
//! `compose:dynlink/linker@0.1.0` WIT package (*"dlopen for wasm
//! components"*) written against `wasmos-runtime-api`.
//!
//! ## What this crate provides
//!
//! A host that installs the `compose:dynlink/linker` interface on a
//! guest via `HostImports`, plus a resident-provider lifecycle
//! ([`ResidentBackend`] + [`ProviderRegistry`]) that compiles a
//! provider component once and reuses its instance across every
//! resolve/invoke call.
//!
//! ## Guest-visible shape
//!
//! ```wit
//! interface linker {
//!     resource instance {
//!         invoke: func(method: string, payload: list<u8>) -> result<list<u8>, error>;
//!     }
//!     resolve-by-id: func(id: string) -> result<instance, error>;
//!     resolve-by-digest: func(d: digest) -> result<instance, error>;
//! }
//! ```
//!
//! ## Consumer usage
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use datalink_dynlink::{install_host_imports, ProviderRegistry, ResidentBackend};
//! use wasmos_runtime_api::HostImports;
//! use wasmos_runtime_api::Runtime;
//!
//! # async fn example(runtime: Arc<dyn Runtime>) -> anyhow::Result<()> {
//! let registry = ProviderRegistry::new(runtime.clone());
//! registry.register_provider("greet", "/path/to/greet-provider.wasm").await?;
//! let backend = Arc::new(ResidentBackend::new(registry));
//! let host_imports = install_host_imports(HostImports::new(), backend);
//! // Pass host_imports into runtime.instantiate() alongside the guest.
//! # Ok(()) }
//! ```
//!
//! ## Version 0.2.0 — ADR-0029 Phase 6.2.c rewrite
//!
//! This crate was rewritten in ADR-0029 Phase 6.2.c to consume
//! `wasmos-runtime-api` exclusively; no direct wasmtime dependency
//! remains. The public API changed shape from the 0.1.x
//! wasmtime-linker installers + bindgen re-exports to the
//! `HostImports`-based `install_host_imports` pattern. Consumers
//! (ducklink, sqlink) migrate in Phases 6.2.d + 6.2.e.
//!
//! See `docs/design/runtime-abstraction/phase-6-2-datalink-dynlink-disposition.md`
//! in the wasmos repo for the full migration plan.

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex as AsyncMutex;

use wasmos_runtime_api::{
    ComponentSource, ExecutionContext, HostCall, HostCallContext, HostImports, Instance, Runtime,
    RuntimeError, RuntimeResult, Value, WasiEnvironment,
};
use wasmos_runtime_api::wasi::Preopen;

/// The runtime-agnostic handle this crate uses. Consumers pick which
/// wasmos runtime implementation to build against (wasmtime v48, edge,
/// future WAMR/browser adapters); this crate consumes only the
/// [`Runtime`] trait surface, keeping `cargo tree -p datalink-dynlink |
/// grep -c wasmtime` at 0.
pub type RuntimeArc = Arc<dyn Runtime>;

/// The WIT interface name this crate installs handlers for.
pub const LINKER_INTERFACE: &str = "compose:dynlink/linker@0.1.0";

/// The WIT resource name within the linker interface — matches the
/// `resource instance { ... }` declaration in the WIT source.
pub const INSTANCE_RESOURCE: &str = "instance";

/// The WIT method-name convention wasmtime uses for methods on
/// resource types: `[method]<resource>.<method-name>`.
///
/// The [`DynLinkBridge`] dispatcher matches this exact name for
/// the guest's `invoke` call on an `instance` handle.
pub const INVOKE_METHOD: &str = "[method]instance.invoke";

// ─── Error type ─────────────────────────────────────────────────────
//
// Mirrors the `sys:compose/types@1.0.0.error` shape from
// `wit/compose-dynlink/deps/sys-compose/types.wit`.

/// Hierarchical error codes matching `sys:compose/types.error-code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // Names are self-explanatory; wit_name() carries the WIT canonical form.
pub enum ErrorCode {
    PlanInvalidSchema,
    PlanInvalidCbor,
    PlanMissingField,
    PlanInvalidGraph,
    PlanCycleDetected,
    EmitMissingBlob,
    EmitInvalidDigest,
    EmitCompositionFailed,
    EmitLinkError,
    ExecTrap,
    ExecTimeout,
    ExecResourceExhausted,
    ExecCapabilityDenied,
    ExecMissingExport,
    BlobNotFound,
    BlobDigestMismatch,
    BlobIoError,
    TrustVerificationFailed,
    TrustSignatureInvalid,
    TrustCertificateExpired,
    TrustUntrustedSource,
    SecretNotFound,
    SecretAccessDenied,
    SecretBackendError,
    InvalidInput,
    InternalError,
    NotImplemented,
}

impl ErrorCode {
    /// The WIT canonical name — used as the [`Value::Variant`]
    /// discriminant string.
    pub fn wit_name(self) -> &'static str {
        match self {
            Self::PlanInvalidSchema => "plan-invalid-schema",
            Self::PlanInvalidCbor => "plan-invalid-cbor",
            Self::PlanMissingField => "plan-missing-field",
            Self::PlanInvalidGraph => "plan-invalid-graph",
            Self::PlanCycleDetected => "plan-cycle-detected",
            Self::EmitMissingBlob => "emit-missing-blob",
            Self::EmitInvalidDigest => "emit-invalid-digest",
            Self::EmitCompositionFailed => "emit-composition-failed",
            Self::EmitLinkError => "emit-link-error",
            Self::ExecTrap => "exec-trap",
            Self::ExecTimeout => "exec-timeout",
            Self::ExecResourceExhausted => "exec-resource-exhausted",
            Self::ExecCapabilityDenied => "exec-capability-denied",
            Self::ExecMissingExport => "exec-missing-export",
            Self::BlobNotFound => "blob-not-found",
            Self::BlobDigestMismatch => "blob-digest-mismatch",
            Self::BlobIoError => "blob-io-error",
            Self::TrustVerificationFailed => "trust-verification-failed",
            Self::TrustSignatureInvalid => "trust-signature-invalid",
            Self::TrustCertificateExpired => "trust-certificate-expired",
            Self::TrustUntrustedSource => "trust-untrusted-source",
            Self::SecretNotFound => "secret-not-found",
            Self::SecretAccessDenied => "secret-access-denied",
            Self::SecretBackendError => "secret-backend-error",
            Self::InvalidInput => "invalid-input",
            Self::InternalError => "internal-error",
            Self::NotImplemented => "not-implemented",
        }
    }
}

/// A structured error matching `sys:compose/types.error`.
#[derive(Clone, Debug)]
pub struct Error {
    /// The error kind.
    pub code: ErrorCode,
    /// A human-readable message.
    pub message: String,
    /// Optional structured context (JSON-serializable, opaque here).
    pub context: Option<String>,
}

impl Error {
    /// Construct an [`Error`] without context.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), context: None }
    }

    /// Marshal to [`Value::Record`] matching the WIT `error` record.
    pub fn to_value(&self) -> Value {
        Value::Record(vec![
            (
                "code".to_string(),
                Value::Variant {
                    discriminant: self.code.wit_name().to_string(),
                    payload: None,
                },
            ),
            ("message".to_string(), Value::String(self.message.clone())),
            (
                "context".to_string(),
                Value::Option(self.context.clone().map(|s| Box::new(Value::String(s)))),
            ),
        ])
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.wit_name(), self.message)
    }
}

impl std::error::Error for Error {}

// ─── ProviderBackend trait ──────────────────────────────────────────

/// The provider-lifecycle contract that a backend implements to
/// serve resolve-by-id / resolve-by-digest / invoke requests routed
/// through the [`DynLinkBridge`].
#[async_trait]
pub trait ProviderBackend: Send + Sync + 'static {
    /// The opaque per-handle state this backend owns.
    type Handle: Clone + Send + Sync + 'static;

    /// Resolve a provider by registry id.
    async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error>;

    /// Resolve a provider by content digest.
    async fn resolve_by_digest(&self, digest: &[u8]) -> Result<Self::Handle, Error>;

    /// Forward an `invoke(method, payload)` call to the provider
    /// this handle points at.
    async fn invoke(
        &self,
        handle: &Self::Handle,
        method: &str,
        payload: Bytes,
    ) -> Result<Bytes, Error>;
}

// ─── DynLinkBridge ──────────────────────────────────────────────────

/// The [`HostCall`] implementation that routes
/// `compose:dynlink/linker` method calls to a [`ProviderBackend`].
///
/// Handle table: maps freshly-minted `u32` rep values to
/// backend-owned `Handle` values. The rep becomes the wasmtime
/// resource's internal representation via
/// [`HostCallContext::new_host_resource`]; the guest observes it as
/// an opaque `Resource<Instance>` handle.
///
/// TODO(phase-6.2.b.3): the `invoke` dispatch needs a way to
/// recover the rep from a `Value::Resource` received as an arg from
/// the guest. Currently uses `handle_id as u32` as a proxy, which
/// works ONLY when the abstraction's assigned handle_id happens to
/// match the rep we requested. Integration testing will surface
/// the divergence; the fix is either a new
/// `HostCallContext::resolve_host_resource(&Value)` method or a
/// `rep: Option<u32>` field on Value::Resource for host-owned
/// handles.
pub struct DynLinkBridge<B: ProviderBackend> {
    backend: Arc<B>,
    handles: AsyncMutex<HandleTable<B::Handle>>,
}

struct HandleTable<H> {
    next_rep: u32,
    map: HashMap<u32, H>,
}

impl<H> HandleTable<H> {
    fn new() -> Self {
        Self { next_rep: 1, map: HashMap::new() }
    }

    fn insert(&mut self, handle: H) -> u32 {
        // Skip zero — some wasmtime code paths treat rep=0 as
        // null-ish; safer to start from 1.
        let rep = self.next_rep;
        self.next_rep = self.next_rep.wrapping_add(1);
        if self.next_rep == 0 {
            self.next_rep = 1;
        }
        self.map.insert(rep, handle);
        rep
    }

    fn get(&self, rep: u32) -> Option<&H> {
        self.map.get(&rep)
    }
}

impl<B: ProviderBackend> DynLinkBridge<B> {
    /// Construct a fresh bridge over the given backend.
    pub fn new(backend: Arc<B>) -> Arc<Self> {
        Arc::new(Self { backend, handles: AsyncMutex::new(HandleTable::new()) })
    }
}

#[async_trait]
impl<B: ProviderBackend> HostCall for DynLinkBridge<B> {
    async fn call(
        &self,
        ctx: &mut HostCallContext<'_>,
        method: &str,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        match method {
            "resolve-by-id" => self.dispatch_resolve_by_id(ctx, args).await,
            "resolve-by-digest" => self.dispatch_resolve_by_digest(ctx, args).await,
            m if m == INVOKE_METHOD => self.dispatch_invoke(args).await,
            // Drop is handled by the adapter's no-op destructor
            // (Phase 6.2.b registered resource types with a no-op
            // dtor). If a future adapter routes drop through here,
            // we can add a "[drop]instance" arm.
            other => Err(RuntimeError::msg(format!(
                "compose:dynlink/linker: unknown method `{other}` — the abstraction \
                 dispatched a method name this bridge does not implement"
            ))),
        }
    }
}

impl<B: ProviderBackend> DynLinkBridge<B> {
    async fn dispatch_resolve_by_id(
        &self,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        let id = match args.as_slice() {
            [Value::String(s)] => s.clone(),
            other => {
                return Err(RuntimeError::msg(format!(
                    "resolve-by-id expected [Value::String(_)], got {other:?}"
                )));
            }
        };
        match self.backend.resolve_by_id(&id).await {
            Ok(handle) => {
                let rep = {
                    let mut table = self.handles.lock().await;
                    table.insert(handle)
                };
                let resource = ctx.new_host_resource(LINKER_INTERFACE, INSTANCE_RESOURCE, rep)?;
                Ok(vec![Value::Result(Ok(Some(Box::new(resource))))])
            }
            Err(e) => Ok(vec![Value::Result(Err(Some(Box::new(e.to_value()))))]),
        }
    }

    async fn dispatch_resolve_by_digest(
        &self,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> RuntimeResult<Vec<Value>> {
        let digest = match args.as_slice() {
            [Value::Bytes(b)] => b.clone(),
            other => {
                return Err(RuntimeError::msg(format!(
                    "resolve-by-digest expected [Value::Bytes(_)], got {other:?}"
                )));
            }
        };
        match self.backend.resolve_by_digest(&digest).await {
            Ok(handle) => {
                let rep = {
                    let mut table = self.handles.lock().await;
                    table.insert(handle)
                };
                let resource = ctx.new_host_resource(LINKER_INTERFACE, INSTANCE_RESOURCE, rep)?;
                Ok(vec![Value::Result(Ok(Some(Box::new(resource))))])
            }
            Err(e) => Ok(vec![Value::Result(Err(Some(Box::new(e.to_value()))))]),
        }
    }

    async fn dispatch_invoke(&self, args: Vec<Value>) -> RuntimeResult<Vec<Value>> {
        // TODO(phase-6.2.b.3): recover the wasmtime rep from
        // Value::Resource received as an arg. Currently uses
        // handle_id-as-rep, which is a placeholder — will need a
        // proper HostCallContext helper (or Value::Resource
        // extension) once end-to-end guest calls exercise this
        // path.
        let (rep_as_handle_id, method, payload) = match args.as_slice() {
            [Value::Resource { handle_id, .. }, Value::String(m), Value::Bytes(p)] => {
                (*handle_id as u32, m.clone(), p.clone())
            }
            other => {
                return Err(RuntimeError::msg(format!(
                    "invoke expected [Resource, String, Bytes], got {other:?}"
                )));
            }
        };
        let handle = {
            let table = self.handles.lock().await;
            table
                .get(rep_as_handle_id)
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::msg(format!(
                        "invoke: unknown handle rep {rep_as_handle_id} (see \
                         phase-6.2.b.3 TODO in datalink-dynlink lib.rs)"
                    ))
                })?
        };
        let result = match self.backend.invoke(&handle, &method, payload).await {
            Ok(bytes) => Value::Result(Ok(Some(Box::new(Value::Bytes(bytes))))),
            Err(e) => Value::Result(Err(Some(Box::new(e.to_value())))),
        };
        Ok(vec![result])
    }
}

/// Install the `compose:dynlink/linker` handler on the given
/// [`HostImports`], returning the updated builder.
pub fn install_host_imports<B: ProviderBackend>(
    host_imports: HostImports,
    backend: Arc<B>,
) -> HostImports {
    let bridge = DynLinkBridge::new(backend);
    host_imports.register(LINKER_INTERFACE, bridge)
}

// ─── ProviderPreopen ────────────────────────────────────────────────

/// A directory to preopen into a provider's own WASI environment.
///
/// Preopens are per-provider — each resident provider's Instance is
/// built with its own [`WasiEnvironment`] containing these preopens.
/// Distinct from the guest's preopens.
#[derive(Clone, Debug)]
pub struct ProviderPreopen {
    /// Host filesystem path to expose.
    pub host: PathBuf,
    /// Path the guest observes (mounted as this prefix).
    pub guest: String,
    /// Read-only vs read-write. Defaults to read-only.
    pub read_only: bool,
}

impl ProviderPreopen {
    /// Read-only preopen at the given host + guest path.
    pub fn read_only(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self { host: host.into(), guest: guest.into(), read_only: true }
    }

    /// Read-write preopen at the given host + guest path.
    pub fn read_write(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self { host: host.into(), guest: guest.into(), read_only: false }
    }
}

// ─── ProviderRegistry ───────────────────────────────────────────────

/// Registry of provider components indexed by id and content digest.
#[derive(Clone)]
pub struct ProviderRegistry {
    runtime: RuntimeArc,
    inner: Arc<AsyncMutex<RegistryInner>>,
}

struct RegistryInner {
    providers: HashMap<String, ProviderEntry>,
    digest_to_id: HashMap<Vec<u8>, String>,
}

struct ProviderEntry {
    #[allow(dead_code)]
    id: String,
    component: wasmos_runtime_api::CompiledComponent,
    preopens: Vec<ProviderPreopen>,
    allow_network: bool,
}

impl ProviderRegistry {
    /// Construct a registry over the shared runtime.
    pub fn new(runtime: RuntimeArc) -> Self {
        Self {
            runtime,
            inner: Arc::new(AsyncMutex::new(RegistryInner {
                providers: HashMap::new(),
                digest_to_id: HashMap::new(),
            })),
        }
    }

    /// Register a provider by id, reading + compiling the wasm bytes
    /// at `path`. Preopens: none, network: disabled by default.
    pub async fn register_provider(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(), Error> {
        self.register_provider_with_options(id, path, Vec::new(), false).await
    }

    /// Register with preopens.
    pub async fn register_provider_with_preopens(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
    ) -> Result<(), Error> {
        self.register_provider_with_options(id, path, preopens, false).await
    }

    /// Register with outbound network access enabled.
    pub async fn register_provider_with_network(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(), Error> {
        self.register_provider_with_options(id, path, Vec::new(), true).await
    }

    /// Register with full options.
    pub async fn register_provider_with_options(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
        allow_network: bool,
    ) -> Result<(), Error> {
        let id = id.into();
        let path = path.into();
        let bytes = std::fs::read(&path).map_err(|e| {
            Error::new(
                ErrorCode::BlobIoError,
                format!("read provider bytes at {}: {e}", path.display()),
            )
        })?;
        let component = self
            .runtime
            .compile_component(
                ComponentSource::Bytes { bytes: bytes.into(), name: Some(id.clone()) },
                Default::default(),
            )
            .await
            .map_err(|e| Error::new(ErrorCode::EmitLinkError, format!("compile provider {id}: {e}")))?;
        let mut inner = self.inner.lock().await;
        inner
            .providers
            .insert(id.clone(), ProviderEntry { id, component, preopens, allow_network });
        Ok(())
    }

    /// Register a digest → id mapping.
    pub async fn register_digest(&self, digest: Vec<u8>, id: impl Into<String>) {
        let mut inner = self.inner.lock().await;
        inner.digest_to_id.insert(digest, id.into());
    }

    async fn get_by_id(
        &self,
        id: &str,
    ) -> Option<(wasmos_runtime_api::CompiledComponent, WasiEnvironment)> {
        let inner = self.inner.lock().await;
        let entry = inner.providers.get(id)?;
        Some((entry.component.clone(), wasi_env_for(&entry.preopens, entry.allow_network)))
    }
}

fn wasi_env_for(preopens: &[ProviderPreopen], allow_network: bool) -> WasiEnvironment {
    let mut env = WasiEnvironment::sandboxed();
    for p in preopens {
        env.preopens.push(Preopen {
            host_path: p.host.clone(),
            guest_path: p.guest.clone(),
            read_only: p.read_only,
        });
    }
    if allow_network {
        env.allow_network = true;
        env.allow_ip_name_lookup = true;
    }
    env
}

// ─── ResidentBackend ────────────────────────────────────────────────

/// A [`ProviderBackend`] that instantiates each provider ONCE (lazily,
/// on first resolve) into a single reusable [`Instance`] shared
/// across every subsequent resolve/invoke for the same id or digest.
#[derive(Clone)]
pub struct ResidentBackend {
    registry: ProviderRegistry,
    slots: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<Option<Arc<AsyncMutex<Instance>>>>>>>>,
}

/// The opaque handle type [`ResidentBackend`] uses.
#[derive(Clone)]
pub struct ResidentHandle {
    id: String,
    slot: Arc<AsyncMutex<Option<Arc<AsyncMutex<Instance>>>>>,
    registry: ProviderRegistry,
}

impl ResidentBackend {
    /// Construct a new resident backend over the given registry.
    pub fn new(registry: ProviderRegistry) -> Self {
        Self { registry, slots: Arc::new(AsyncMutex::new(HashMap::new())) }
    }

    /// Access the underlying registry.
    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    async fn slot_for(&self, id: &str) -> Arc<AsyncMutex<Option<Arc<AsyncMutex<Instance>>>>> {
        let mut slots = self.slots.lock().await;
        slots.entry(id.to_string()).or_insert_with(|| Arc::new(AsyncMutex::new(None))).clone()
    }
}

#[async_trait]
impl ProviderBackend for ResidentBackend {
    type Handle = ResidentHandle;

    async fn resolve_by_id(&self, id: &str) -> Result<Self::Handle, Error> {
        if self.registry.get_by_id(id).await.is_none() {
            return Err(Error::new(
                ErrorCode::BlobNotFound,
                format!("no provider registered under id {id:?}"),
            ));
        }
        let slot = self.slot_for(id).await;
        Ok(ResidentHandle { id: id.to_string(), slot, registry: self.registry.clone() })
    }

    async fn resolve_by_digest(&self, digest: &[u8]) -> Result<Self::Handle, Error> {
        let inner = self.registry.inner.lock().await;
        let id = inner
            .digest_to_id
            .get(digest)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::BlobNotFound,
                    format!("no provider registered for digest {:02x?}", digest),
                )
            })?
            .clone();
        drop(inner);
        self.resolve_by_id(&id).await
    }

    async fn invoke(
        &self,
        handle: &Self::Handle,
        method: &str,
        payload: Bytes,
    ) -> Result<Bytes, Error> {
        // Materialize the instance lazily on first invoke.
        let inst_mutex = {
            let mut slot = handle.slot.lock().await;
            match slot.as_ref() {
                Some(inst) => inst.clone(),
                None => {
                    let (component, env) = handle.registry.get_by_id(&handle.id).await.ok_or_else(
                        || {
                            Error::new(
                                ErrorCode::BlobNotFound,
                                format!("provider {} vanished from registry", handle.id),
                            )
                        },
                    )?;
                    let instance = handle
                        .registry
                        .runtime
                        .instantiate(&component, ExecutionContext::new().with_wasi(env))
                        .await
                        .map_err(|e| {
                            Error::new(
                                ErrorCode::EmitLinkError,
                                format!("instantiate provider {}: {e}", handle.id),
                            )
                        })?;
                    let inst = Arc::new(AsyncMutex::new(instance));
                    *slot = Some(inst.clone());
                    inst
                }
            }
        };
        // Call the provider's compose:dynlink/endpoint.handle export.
        let mut instance = inst_mutex.lock().await;
        let result = instance
            .call_export(
                "compose:dynlink/endpoint@0.1.0#handle",
                &[Value::String(method.to_string()), Value::Bytes(payload)],
            )
            .await
            .map_err(|e| {
                Error::new(
                    ErrorCode::ExecTrap,
                    format!("provider {}.handle failed: {e}", handle.id),
                )
            })?;
        // Provider returns `result<list<u8>, error>` — one Value.
        match result.into_iter().next() {
            Some(Value::Result(Ok(Some(payload)))) => match *payload {
                Value::Bytes(b) => Ok(b),
                other => Err(Error::new(
                    ErrorCode::InternalError,
                    format!("provider returned non-bytes payload: {other:?}"),
                )),
            },
            Some(Value::Result(Ok(None))) => Ok(Bytes::new()),
            Some(Value::Result(Err(_))) => Err(Error::new(
                ErrorCode::ExecTrap,
                format!("provider {} returned error", handle.id),
            )),
            other => Err(Error::new(
                ErrorCode::InternalError,
                format!("provider {} returned unexpected shape: {other:?}", handle.id),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_marshals_to_expected_record_shape() {
        let e = Error::new(ErrorCode::BlobNotFound, "no such provider");
        match e.to_value() {
            Value::Record(fields) => {
                assert_eq!(fields.len(), 3);
                assert_eq!(fields[0].0, "code");
                assert_eq!(fields[1].0, "message");
                assert_eq!(fields[2].0, "context");
                match &fields[0].1 {
                    Value::Variant { discriminant, payload } => {
                        assert_eq!(discriminant, "blob-not-found");
                        assert!(payload.is_none());
                    }
                    other => panic!("expected code variant, got {other:?}"),
                }
                match &fields[1].1 {
                    Value::String(s) => assert_eq!(s, "no such provider"),
                    other => panic!("expected message string, got {other:?}"),
                }
                match &fields[2].1 {
                    Value::Option(None) => {}
                    other => panic!("expected context none, got {other:?}"),
                }
            }
            other => panic!("expected Value::Record, got {other:?}"),
        }
    }

    #[test]
    fn error_code_wit_names_cover_all_variants() {
        // Smoke: every variant produces a kebab-case name.
        let all = [
            ErrorCode::PlanInvalidSchema,
            ErrorCode::PlanInvalidCbor,
            ErrorCode::PlanMissingField,
            ErrorCode::PlanInvalidGraph,
            ErrorCode::PlanCycleDetected,
            ErrorCode::EmitMissingBlob,
            ErrorCode::EmitInvalidDigest,
            ErrorCode::EmitCompositionFailed,
            ErrorCode::EmitLinkError,
            ErrorCode::ExecTrap,
            ErrorCode::ExecTimeout,
            ErrorCode::ExecResourceExhausted,
            ErrorCode::ExecCapabilityDenied,
            ErrorCode::ExecMissingExport,
            ErrorCode::BlobNotFound,
            ErrorCode::BlobDigestMismatch,
            ErrorCode::BlobIoError,
            ErrorCode::TrustVerificationFailed,
            ErrorCode::TrustSignatureInvalid,
            ErrorCode::TrustCertificateExpired,
            ErrorCode::TrustUntrustedSource,
            ErrorCode::SecretNotFound,
            ErrorCode::SecretAccessDenied,
            ErrorCode::SecretBackendError,
            ErrorCode::InvalidInput,
            ErrorCode::InternalError,
            ErrorCode::NotImplemented,
        ];
        for code in all {
            let name = code.wit_name();
            assert!(!name.is_empty());
            assert!(!name.contains(' '), "wit_name should be kebab-case, got {name:?}");
        }
    }
}
