//! Async, deep-reentrant + streaming `compose:dynlink` host machinery.
//!
//! This is the ASYNC port of the consolidated reference host
//! (`webassembly-component-orchestration`,
//! `hosts/wasmtime/src/dynlink.rs` on `feat/dynlink-loader-integration`),
//! adapted from sync wasmtime to the async wasmtime / async linker that
//! sqlink's production host runs on. It is the foundation #226 rewires
//! sqlink's bespoke extension-loader onto.
//!
//! It consolidates the three proven features:
//!
//!  1. **Deep reentrancy (#221).** Every resolved provider runs in its OWN
//!     [`Store<DynState>`] (not a WASI-only provider state). The provider
//!     store's linker carries WASI **and** the `compose:dynlink/linker`
//!     bridge, so a provider may itself import `linker` and re-enter the
//!     host mid-`handle` to resolve/invoke further providers. The nested
//!     host call borrows the PROVIDER store's `DynState` — a distinct
//!     object from the outer guest's — so there is no aliasing across the
//!     nesting boundary and no interior mutability is needed.
//!     [`DynState::child_state`] mints a fresh per-provider `DynState`
//!     (own `dyn_table` + WASI, shared engine/blobs/trust/registry/linkers,
//!     grant snapshotted).
//!
//!  2. **Streaming dot-commands (#223).** A streaming-dotcmd provider emits
//!     through the `sqlite:extension/cli-stdout` host import (which `wac
//!     plug` leaves dangling, the cli-stream cycle) rather than the invoke
//!     envelope. The host detects that import shape on resolve
//!     ([`ProviderKind::Cli`] vs [`ProviderKind::Plain`]), instantiates the
//!     provider with the cli-aware linker (WASI + dynlink bridge + the
//!     host-mediated cli-stdout/stderr/state), captures the streamed bytes
//!     in a per-instance [`CliCapture`], and host-answers the reserved
//!     `cli.drain-stdout` / `cli.drain-stderr` invoke methods.
//!
//!  3. **Engine-as-provider.** Because the registry (id -> digest) is shared
//!     into every child `DynState`, a reentrant extension provider can
//!     `resolve_by_id("engine")` the SPI-leaf engine and invoke it, and the
//!     engine provider can in turn `resolve_by_id("ext")` — full
//!     bidirectional deep reentrancy.
//!
//! ## Async-reentrancy correctness
//!
//! The one real risk in the sync->async port is that the provider re-enters
//! the linker mid-`handle` across `.await` points. wasmtime drives a nested
//! component call INLINE within the same future poll chain — it is not a
//! separate task and does not take a second lock on the same store. The
//! outer async [`HostInstance::invoke`] holds `&mut self` (the guest's
//! `DynState`) parked across `call_handle(&mut di.store, ..).await`; the
//! nested host call wasmtime dispatches operates on `di.store.data_mut()`
//! (the provider's `DynState`), a DISTINCT object. Each `Store<DynState>`
//! is therefore touched by exactly one stack frame at a time, exactly as in
//! the sync version — the nested-stack-dispatch property is preserved by the
//! async executor, with no store/executor deadlock. (There is intentionally
//! no `tokio::sync::Mutex` over these stores: each provider store is owned
//! by its `DynInstance` in the resolving state's `dyn_table`, single-owner,
//! so there is nothing to serialize and nothing to deadlock.)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Capability a plan must be granted to resolve (instantiate) a component
/// at runtime.
pub const CAP_RESOLVE: &str = "dynlink:resolve";
/// Capability a plan must be granted to invoke a resolved instance.
pub const CAP_INVOKE: &str = "dynlink:invoke";

/// The runtime-linking `linker` interface name (version-agnostic prefix).
pub const LINKER_INTERFACE: &str = "compose:dynlink/linker";
/// The `sqlite:extension/cli-stdout` import a STREAMING dot-command leaves
/// dangling after `wac plug` — the host satisfies and captures it.
const CLI_STDOUT_INTERFACE: &str = "sqlite:extension/cli-stdout";

/// Determinism mode of the executing plan. Runtime linking is a
/// non-deterministic operation, so it is refused under `Strict`. Mirrors
/// the reference host's `compose_core::types::DeterminismMode` (the
/// datalink crate carries its own copy so it has no compose_core dep).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeterminismMode {
    Strict,
    Audit,
    Relaxed,
}

// ── Async bindgen worlds ───────────────────────────────────────────────
//
// All async (`default: async`) so the host methods are `async fn` and the
// provider `endpoint.handle` / guest `wasi:cli/run` calls are `.await`ed on
// the async executor — the production sqlink shape.

/// Guest-facing bindings: the host satisfies a guest's
/// `compose:dynlink/linker` import. Async host methods.
mod guest {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-guest",
        imports: { default: async },
    });
}

/// PLAIN provider bindings (WASI + dynlink-only imports; exports endpoint).
mod provider {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-provider",
        imports: { default: async },
        exports: { default: async },
    });
}

/// STREAMING dot-command provider bindings: exports endpoint AND imports the
/// host-mediated `sqlite:extension/cli-*` streams. Async host + exports.
mod provider_cli {
    wasmtime::component::bindgen!({
        path: "wit/compose-dynlink",
        world: "dynlink-provider-cli",
        imports: { default: async },
        exports: { default: async },
    });
}

use guest::compose::dynlink::linker::{Host as LinkerHost, HostInstance, Instance};
use guest::sys::compose::types::{Error, ErrorCode};

/// Build a guest-facing `Error`.
fn error(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        context: None,
    }
}

/// Lower a plain provider endpoint error (distinct generated type, same
/// shape) into the guest-facing `Error`.
fn lower_provider_error(e: provider::sys::compose::types::Error) -> Error {
    Error {
        code: ErrorCode::ExecTrap,
        message: format!("provider endpoint error: {}", e.message),
        context: e.context,
    }
}

/// Lower a streaming-dotcmd provider endpoint error (a distinct generated
/// type from the plain provider's) into the guest-facing `Error`.
fn lower_provider_cli_error(e: provider_cli::sys::compose::types::Error) -> Error {
    Error {
        code: ErrorCode::ExecTrap,
        message: format!("provider endpoint error: {}", e.message),
        context: e.context,
    }
}

/// Retype a guest-facing `Resource<Instance>` to the host backing type. The
/// two share the same table rep — the type parameter is only a host-side
/// compile-time tag — so this is a sound reinterpretation.
fn as_backing(r: &Resource<Instance>) -> Resource<DynInstance> {
    Resource::new_own(r.rep())
}

/// Convenience for the bindgen-generated `add_to_linker` signature.
struct HasSelf<T>(std::marker::PhantomData<T>);
impl<T: 'static> wasmtime::component::HasData for HasSelf<T> {
    type Data<'a> = &'a mut T;
}

// ── Provider-side trust + blob backing ─────────────────────────────────
//
// The reference host gates resolution on a content-addressed BlobStore +
// TrustStore. datalink-dynlink stays storage-agnostic: it takes the two as
// trait objects so sqlink can plug its real CAS + trust gate (or, in tests,
// an in-memory map). The guest resolves by id; the registry maps id ->
// digest; the loader fetches+verifies the digest.

/// Content-addressed blob source: digest -> component bytes.
pub trait BlobSource: Send + Sync {
    fn get(&self, digest: &[u8]) -> Result<Vec<u8>, String>;
}

/// Trust gate: a digest must verify before it is instantiated.
pub trait TrustGate: Send + Sync {
    fn verify(&self, digest: &[u8]) -> Result<(), String>;
}

/// An in-memory blob+trust backing for tests/simple hosts: every inserted
/// digest is both fetchable and trusted.
#[derive(Clone, Default)]
pub struct MemBlobs {
    map: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
}

impl MemBlobs {
    pub fn new() -> Self {
        Self::default()
    }
    /// Store `bytes` and return a content digest (sha-256-free: a stable
    /// length-prefixed identity hash is enough for the registry mapping;
    /// callers that need real CAS supply their own [`BlobSource`]).
    pub fn put(&self, bytes: &[u8]) -> Vec<u8> {
        let digest = blake_like_digest(bytes);
        self.map
            .lock()
            .unwrap()
            .insert(digest.clone(), bytes.to_vec());
        digest
    }
}

impl BlobSource for MemBlobs {
    fn get(&self, digest: &[u8]) -> Result<Vec<u8>, String> {
        self.map
            .lock()
            .unwrap()
            .get(digest)
            .cloned()
            .ok_or_else(|| "blob not found".to_string())
    }
}

impl TrustGate for MemBlobs {
    fn verify(&self, digest: &[u8]) -> Result<(), String> {
        if self.map.lock().unwrap().contains_key(digest) {
            Ok(())
        } else {
            Err("untrusted digest".to_string())
        }
    }
}

/// A non-cryptographic content identity for the in-memory store. Real hosts
/// pass their own `BlobSource`/`TrustGate` (sqlink's CAS sha-256). FNV-1a
/// over the bytes, widened to 32 bytes, is collision-resistant enough to
/// key a test registry.
fn blake_like_digest(bytes: &[u8]) -> Vec<u8> {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut out = Vec::with_capacity(32);
    for i in 0..4u64 {
        out.extend_from_slice(&(h ^ i.wrapping_mul(0x9e3779b97f4a7c15)).to_le_bytes());
    }
    out
}

// ── CLI capture (streaming dot-commands, #223) ─────────────────────────

/// Host-owned capture sink for a streaming dot-command's CLI streams. The
/// host implements `sqlite:extension/cli-stdout`/`-stderr`/`-state` on the
/// provider's store; a streaming command's writes land here. Behind an
/// `Arc<Mutex<_>>` so the resolving caller keeps a handle to drain it after
/// the run without reaching into the provider's store.
#[derive(Clone, Default)]
pub struct CliCapture {
    inner: Arc<Mutex<CliCaptureInner>>,
}

#[derive(Default)]
struct CliCaptureInner {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    state: BTreeMap<String, String>,
}

impl CliCapture {
    pub fn take_stdout(&self) -> Vec<u8> {
        std::mem::take(&mut self.inner.lock().unwrap().stdout)
    }
    pub fn take_stderr(&self) -> Vec<u8> {
        std::mem::take(&mut self.inner.lock().unwrap().stderr)
    }
    fn push_stdout(&self, b: &[u8]) {
        self.inner.lock().unwrap().stdout.extend_from_slice(b);
    }
    fn push_stderr(&self, b: &[u8]) {
        self.inner.lock().unwrap().stderr.extend_from_slice(b);
    }
    fn state_text(&self, key: &str) -> String {
        self.inner
            .lock()
            .unwrap()
            .state
            .get(key)
            .cloned()
            .unwrap_or_default()
    }
    fn state_keys(&self, prefix: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .state
            .keys()
            .filter(|k| prefix.is_empty() || k.starts_with(prefix))
            .cloned()
            .collect()
    }
}

// ── Host-mediated CLI streams on DynState ──────────────────────────────
//
// The host IS the cli for a plugged streaming extension: it implements the
// `sqlite:extension/cli-*` imports `wac plug` left dangling. Output is
// captured in the store's `CliCapture`; the resolving path drains it.
// These are async host methods (the cli-provider world is `default: async`).

impl provider_cli::sqlite::extension::cli_stdout::Host for DynState {
    async fn write(&mut self, text: String) {
        self.cli.push_stdout(text.as_bytes());
    }
    async fn flush(&mut self) {}
    async fn row_end(&mut self) {
        self.cli.push_stdout(b"\n");
    }
}

impl provider_cli::sqlite::extension::cli_stderr::Host for DynState {
    async fn write(&mut self, text: String) {
        self.cli.push_stderr(text.as_bytes());
    }
}

impl provider_cli::sqlite::extension::types::Host for DynState {}

impl provider_cli::sqlite::extension::cli_state::Host for DynState {
    async fn get_text(&mut self, key: String) -> String {
        self.cli.state_text(&key)
    }
    async fn get_int(&mut self, key: String) -> i64 {
        self.cli.state_text(&key).trim().parse().unwrap_or(0)
    }
    async fn get_bool(&mut self, key: String) -> bool {
        matches!(self.cli.state_text(&key).trim(), "true" | "1")
    }
    async fn get_real(&mut self, key: String) -> f64 {
        self.cli.state_text(&key).trim().parse().unwrap_or(0.0)
    }
    async fn get_value(&mut self, key: String) -> provider_cli::sqlite::extension::types::SqlValue {
        use provider_cli::sqlite::extension::types::SqlValue as V;
        let t = self.cli.state_text(&key);
        let t = t.trim();
        if t.is_empty() || t == "null" {
            return V::Null;
        }
        if let Ok(i) = t.parse::<i64>() {
            return V::Integer(i);
        }
        if let Ok(f) = t.parse::<f64>() {
            return V::Real(f);
        }
        V::Text(t.to_string())
    }
    async fn list_keys(&mut self, prefix: String) -> Vec<String> {
        self.cli.state_keys(&prefix)
    }
}

/// A resolved provider's `endpoint` accessor — plain (WASI-only imports) or
/// streaming-dotcmd (host-mediated cli streams). Both export the same
/// `endpoint`; only the bindgen struct + error-lowering type differ.
enum ProviderKind {
    Plain(provider::DynlinkProvider),
    Cli(provider_cli::DynlinkProviderCli),
}

/// A resolved provider, owned by the host and handed to the guest as an
/// opaque `instance` handle. Holds its OWN store carrying its OWN
/// [`DynState`], so a provider that imports `compose:dynlink/linker` can
/// re-enter the linker mid-`handle` (deep nested reentrancy).
pub struct DynInstance {
    store: Store<DynState>,
    provider: ProviderKind,
    #[allow(dead_code)]
    digest: Vec<u8>,
    /// Capabilities snapshotted at resolve time — a resolved component can
    /// never exceed the loader's grant.
    capabilities: BTreeSet<String>,
    /// Shares the `Arc` with the provider store's `DynState.cli`; drained by
    /// `invoke` on a `cli.drain-*` to surface streamed text.
    cli: CliCapture,
}

/// Host-side state shared with every guest call into the dynlink bridge.
/// When this `DynState` backs a PROVIDER store, the provider's nested
/// resolves/invokes route against THIS object (deep reentrancy).
pub struct DynState {
    wasi_ctx: WasiCtx,
    wasi_table: ResourceTable,
    /// Owns the live [`DynInstance`] handles. Distinct from `wasi_table` so
    /// dropping a dynlink handle never disturbs WASI-owned resources.
    dyn_table: ResourceTable,
    engine: Engine,
    blobs: Arc<dyn BlobSource>,
    trust: Arc<dyn TrustGate>,
    /// Linker for a PLAIN provider: WASI + the `compose:dynlink/linker`
    /// bridge over `DynState`, so a resolved provider may itself re-enter.
    /// `Arc`-shared across this state and every provider child it spawns.
    provider_linker: Arc<Linker<DynState>>,
    /// Linker for a STREAMING dot-command provider: the plain reentrant
    /// linker PLUS the host-mediated `sqlite:extension` cli streams.
    provider_cli_linker: Arc<Linker<DynState>>,
    determinism: DeterminismMode,
    granted: BTreeSet<String>,
    /// id -> digest, used by `resolve-by-id`. Shared into children so an
    /// engine provider can resolve "ext" and vice-versa (engine-as-provider).
    registry: HashMap<String, Vec<u8>>,
    resolved: BTreeSet<Vec<u8>>,
    /// Capture sink for THIS state's streaming dotcmd CLI output. Written
    /// only when this `DynState` backs a streaming provider store.
    cli: CliCapture,
}

impl DynState {
    /// Construct a top-level dynlink host state. The provider linkers carry
    /// WASI + the dynlink bridge (so providers can re-enter); the cli linker
    /// also carries the host-mediated cli streams. Async WASI linkers, since
    /// the providers/guests are driven on the async executor.
    pub fn new(
        engine: Engine,
        blobs: Arc<dyn BlobSource>,
        trust: Arc<dyn TrustGate>,
        determinism: DeterminismMode,
        granted: BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        let provider_linker = build_provider_linker(&engine)?;
        let provider_cli_linker = build_provider_cli_linker(&engine)?;
        Ok(Self {
            wasi_ctx: WasiCtxBuilder::new().build(),
            wasi_table: ResourceTable::new(),
            dyn_table: ResourceTable::new(),
            engine,
            blobs,
            trust,
            provider_linker: Arc::new(provider_linker),
            provider_cli_linker: Arc::new(provider_cli_linker),
            determinism,
            granted,
            registry: HashMap::new(),
            resolved: BTreeSet::new(),
            cli: CliCapture::default(),
        })
    }

    /// Mint a fresh `DynState` for a newly-resolved provider's own store.
    /// Shares engine/blobs/trust/registry/linkers (`Arc`), snapshots the
    /// grant, and gets a FRESH `dyn_table` + WASI + `CliCapture`. The fresh
    /// `dyn_table` is what makes nested `invoke` re-entrant: a nested host
    /// call borrows THIS child `DynState`, a distinct object from the parent.
    fn child_state(&self) -> Self {
        Self {
            wasi_ctx: WasiCtxBuilder::new().build(),
            wasi_table: ResourceTable::new(),
            dyn_table: ResourceTable::new(),
            engine: self.engine.clone(),
            blobs: Arc::clone(&self.blobs),
            trust: Arc::clone(&self.trust),
            provider_linker: Arc::clone(&self.provider_linker),
            provider_cli_linker: Arc::clone(&self.provider_cli_linker),
            determinism: self.determinism,
            granted: self.granted.clone(),
            registry: self.registry.clone(),
            resolved: BTreeSet::new(),
            cli: CliCapture::default(),
        }
    }

    /// Register an `id -> digest` mapping for `resolve-by-id`.
    pub fn register_id(&mut self, id: impl Into<String>, digest: Vec<u8>) {
        self.registry.insert(id.into(), digest);
    }

    /// The set of provider digests resolved during this execution, sorted.
    pub fn resolved_providers(&self) -> &BTreeSet<Vec<u8>> {
        &self.resolved
    }
}

impl WasiView for DynState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.wasi_table,
        }
    }
}

impl guest::sys::compose::types::Host for DynState {}

impl LinkerHost for DynState {
    async fn resolve_by_digest(&mut self, d: Vec<u8>) -> Result<Resource<Instance>, Error> {
        // 0. Determinism gate.
        if self.determinism == DeterminismMode::Strict {
            return Err(error(
                ErrorCode::ExecCapabilityDenied,
                "runtime linking is not permitted under strict determinism",
            ));
        }
        // 0b. Capability gate.
        if !self.granted.contains(CAP_RESOLVE) {
            return Err(error(
                ErrorCode::ExecCapabilityDenied,
                format!("resolution requires the '{CAP_RESOLVE}' capability"),
            ));
        }
        // 1. Trust gate.
        self.trust
            .verify(&d)
            .map_err(|e| error(ErrorCode::TrustUntrustedSource, e))?;
        // 2. Load the provider bytes.
        let bytes = self
            .blobs
            .get(&d)
            .map_err(|e| error(ErrorCode::BlobNotFound, e))?;
        // 3. Compile, detect the streaming-dotcmd shape, instantiate in the
        // provider's OWN store carrying its OWN `DynState` (deep reentrancy).
        let component = Component::new(&self.engine, &bytes).map_err(|e| {
            error(
                ErrorCode::EmitLinkError,
                format!("failed to load provider component: {e:?}"),
            )
        })?;
        let child = self.child_state();
        let cli = child.cli.clone();
        let mut store = Store::new(&self.engine, child);
        let provider = if imports_cli_stdout(&self.engine, &component) {
            let instance = provider_cli::DynlinkProviderCli::instantiate_async(
                &mut store,
                &component,
                self.provider_cli_linker.as_ref(),
            )
            .await
            .map_err(|e| {
                error(
                    ErrorCode::ExecTrap,
                    format!("failed to instantiate streaming-dotcmd provider: {e:?}"),
                )
            })?;
            ProviderKind::Cli(instance)
        } else {
            let instance = provider::DynlinkProvider::instantiate_async(
                &mut store,
                &component,
                self.provider_linker.as_ref(),
            )
            .await
            .map_err(|e| {
                error(
                    ErrorCode::ExecTrap,
                    format!("failed to instantiate provider: {e:?}"),
                )
            })?;
            ProviderKind::Plain(instance)
        };
        // 4. Mint the opaque handle.
        let backing = self
            .dyn_table
            .push(DynInstance {
                store,
                provider,
                digest: d.clone(),
                capabilities: self.granted.clone(),
                cli,
            })
            .map_err(|e| {
                error(
                    ErrorCode::InternalError,
                    format!("resource table push failed: {e:?}"),
                )
            })?;
        // 5. Record the resolved digest.
        self.resolved.insert(d);
        Ok(Resource::new_own(backing.rep()))
    }

    async fn resolve_by_id(&mut self, id: String) -> Result<Resource<Instance>, Error> {
        let digest = self
            .registry
            .get(&id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::InvalidInput, format!("unknown component id: {id}")))?;
        self.resolve_by_digest(digest).await
    }
}

impl HostInstance for DynState {
    async fn invoke(
        &mut self,
        self_: Resource<Instance>,
        method: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let di = self.dyn_table.get_mut(&as_backing(&self_)).map_err(|e| {
            error(
                ErrorCode::InternalError,
                format!("unknown dynlink handle: {e:?}"),
            )
        })?;
        // Capability gate.
        if !di.capabilities.contains(CAP_INVOKE) {
            return Err(error(
                ErrorCode::ExecCapabilityDenied,
                format!("invoke requires the '{CAP_INVOKE}' capability"),
            ));
        }
        // Host-mediated CLI drain methods (#223): answered by the HOST, not
        // forwarded to the provider. The guest calls them right after
        // `dotcmd.invoke` to collect what the streaming command emitted.
        match method.as_str() {
            "cli.drain-stdout" => return Ok(di.cli.take_stdout()),
            "cli.drain-stderr" => return Ok(di.cli.take_stderr()),
            _ => {}
        }
        // Forward verbatim. `di.store` is a `Store<DynState>` carrying the
        // provider's OWN `DynState`; if the provider re-enters the linker
        // mid-`handle`, wasmtime dispatches that nested host call against
        // `di.store.data_mut()` (the provider's `DynState`, distinct from
        // `self`), INLINE within this future. `&mut self` is parked across
        // the `.await` but does not alias the provider store's data, so the
        // nested `&mut DynState` is sound and no executor deadlock occurs.
        match &di.provider {
            ProviderKind::Plain(p) => {
                let endpoint = p.compose_dynlink_endpoint();
                let result = endpoint
                    .call_handle(&mut di.store, &method, &payload)
                    .await
                    .map_err(|e| {
                        error(ErrorCode::ExecTrap, format!("provider handle trapped: {e:?}"))
                    })?;
                result.map_err(lower_provider_error)
            }
            ProviderKind::Cli(p) => {
                let endpoint = p.compose_dynlink_endpoint();
                let result = endpoint
                    .call_handle(&mut di.store, &method, &payload)
                    .await
                    .map_err(|e| {
                        error(ErrorCode::ExecTrap, format!("provider handle trapped: {e:?}"))
                    })?;
                result.map_err(lower_provider_cli_error)
            }
        }
    }

    async fn drop(&mut self, rep: Resource<Instance>) -> wasmtime::Result<()> {
        let _ = self.dyn_table.delete(as_backing(&rep))?;
        Ok(())
    }
}

/// Add the `compose:dynlink/linker` import to a linker over `DynState`.
pub fn add_to_linker(linker: &mut Linker<DynState>) -> anyhow::Result<()> {
    guest::DynlinkGuest::add_to_linker::<_, HasSelf<DynState>>(linker, |s| s)
        .map_err(|e| anyhow::anyhow!("failed to add compose:dynlink bindings to linker: {e:?}"))
}

/// Build the PLAIN provider linker: async WASI + the dynlink bridge over
/// `DynState` (so a provider can re-enter).
fn build_provider_linker(engine: &Engine) -> anyhow::Result<Linker<DynState>> {
    let mut linker = Linker::<DynState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| anyhow::anyhow!("failed to add WASI to provider linker: {e:?}"))?;
    add_to_linker(&mut linker)
        .map_err(|e| anyhow::anyhow!("failed to add dynlink bridge to provider linker: {e:?}"))?;
    Ok(linker)
}

/// Build the STREAMING dot-command provider linker: the plain reentrant
/// linker PLUS the host-mediated `sqlite:extension` cli streams.
fn build_provider_cli_linker(engine: &Engine) -> anyhow::Result<Linker<DynState>> {
    let mut linker = build_provider_linker(engine)?;
    use provider_cli::sqlite::extension as ext;
    ext::cli_stdout::add_to_linker::<_, HasSelf<DynState>>(&mut linker, |s| s)
        .map_err(|e| anyhow::anyhow!("failed to add cli-stdout to provider linker: {e:?}"))?;
    ext::cli_stderr::add_to_linker::<_, HasSelf<DynState>>(&mut linker, |s| s)
        .map_err(|e| anyhow::anyhow!("failed to add cli-stderr to provider linker: {e:?}"))?;
    ext::cli_state::add_to_linker::<_, HasSelf<DynState>>(&mut linker, |s| s)
        .map_err(|e| anyhow::anyhow!("failed to add cli-state to provider linker: {e:?}"))?;
    Ok(linker)
}

/// Whether a compiled component imports `compose:dynlink/linker` (flavor B).
pub fn imports_linker(engine: &Engine, component: &Component) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with(LINKER_INTERFACE))
}

/// Whether a compiled provider imports `sqlite:extension/cli-stdout` — the
/// signature of a STREAMING dot-command whose streams the host satisfies.
fn imports_cli_stdout(engine: &Engine, component: &Component) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with(CLI_STDOUT_INTERFACE))
}

/// Output of running a flavor-B dlopen guest CLI.
#[derive(Debug)]
pub struct RunOutput {
    pub exit_code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Provider digests the guest resolved during the run.
    pub resolved: BTreeSet<Vec<u8>>,
}

/// Run a flavor-B dlopen guest CLI (`wasi:cli/run`, imports
/// `compose:dynlink/linker`) on the ASYNC executor. `registry` maps the ids
/// the guest resolves to provider digests; `blobs`/`trust` back resolution.
/// This is the async port of the reference `run_cli_dlopen`, and the shape
/// #226 rewires sqlink's loader onto.
#[allow(clippy::too_many_arguments)]
pub async fn run_cli_dlopen(
    engine: &Engine,
    guest_bytes: &[u8],
    registry: &[(String, Vec<u8>)],
    blobs: Arc<dyn BlobSource>,
    trust: Arc<dyn TrustGate>,
    determinism: DeterminismMode,
    granted: BTreeSet<String>,
    args: &[String],
    env: &[(String, String)],
) -> anyhow::Result<RunOutput> {
    let component = Component::new(engine, guest_bytes)
        .map_err(|e| anyhow::anyhow!("failed to load guest component: {e:?}"))?;
    if !imports_linker(engine, &component) {
        anyhow::bail!("guest does not import {LINKER_INTERFACE}");
    }

    let mut linker = Linker::<DynState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| anyhow::anyhow!("failed to add WASI to guest linker: {e:?}"))?;
    add_to_linker(&mut linker)?;

    let mut state = DynState::new(
        engine.clone(),
        blobs,
        trust,
        determinism,
        granted,
    )?;
    for (id, digest) in registry {
        state.register_id(id.clone(), digest.clone());
    }

    let stdout = MemoryOutputPipe::new(256 * 1024);
    let stderr = MemoryOutputPipe::new(256 * 1024);
    let mut builder = WasiCtxBuilder::new();
    builder.args(args);
    for (k, v) in env {
        builder.env(k, v);
    }
    builder
        .stdin(MemoryInputPipe::new(Vec::new()))
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    state.wasi_ctx = builder.build();

    let mut store = Store::new(engine, state);
    let command =
        wasmtime_wasi::p2::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(|e| anyhow::anyhow!("failed to instantiate guest command: {e:?}"))?;
    let run_result = command.wasi_cli_run().call_run(&mut store).await;
    let exit_code = match run_result {
        Ok(Ok(())) => 0u32,
        Ok(Err(())) => 1u32,
        Err(e) => {
            let tail = String::from_utf8_lossy(&stderr.contents()).trim().to_string();
            if tail.is_empty() {
                anyhow::bail!("guest run trapped: {e:?}");
            } else {
                anyhow::bail!("guest run trapped: {e:?}\nguest stderr:\n{tail}");
            }
        }
    };
    let resolved = store.data().resolved_providers().clone();
    drop(store);
    Ok(RunOutput {
        exit_code,
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
        resolved,
    })
}
