# datalink — consolidation sweep of sqlink + ducklink

`ducklink` (DuckDB→wasm + a catalog of `duckdb:extension` components) was
originally **mirrored from `sqlink`** (SQLite→wasm + `sqlite:extension`
components). The two have since evolved in parallel and now duplicate a large
amount of *DB-agnostic* infrastructure. This sweep identifies what to lift into
`datalink` (shared) vs. what stays DB-specific.

## The fault line

Almost everything divides cleanly along one axis:

- **DB-specific (stays in each repo):** the engine compiled to wasm (DuckDB vs
  SQLite), the component catalog itself, and the **value/type model** — DuckDB's
  rich logical types (int8/16/32, decimals, temporal, uuid, nested) vs SQLite's
  five fixed storage classes. This type difference is the *only* deep semantic
  divergence; it must be a parameter of the shared code, not hard-coded.
- **DB-agnostic (lift to datalink):** the catalog tooling, the registry + identity
  model, the host loader/dispatch/compose/cache/policy patterns, the WIT
  versioning + contract-guard machinery, the browser runtime plumbing, and the
  docs/CI scaffolding. None of this cares whether the engine is DuckDB or SQLite.

## Consolidation inventory (tiered by duplication × value, cheapest first)

### Tier 1 — tooling + identity model (highest duplication, lowest coupling)
The catalog tooling is a near-verbatim mirror. Both repos carry:
`scaffold.py`, `smoke.py`, `t-status.py`, `compat-registry.json`,
`lessons-learned.md` (+ `lessons-stub`), `templates/`, and the registry model.
sqlink adds `plan-add.py`/`next-fid.py`/`bench.py`/`cli-smoke.py`; ducklink adds
`gen-catalog.py`/`verify-catalog.py`/`propagate-wit.py`/`builds.py` — but these are
*generic patterns*, not DB-specific logic.
- **Registry schema** is a shared core — both entries share
  `name, version, description, license, authors, repository, keywords,
  categories, source, exports` — diverging only in DB-tagged fields
  (`min_{duck,sqlite}db_version`, `wit_contract`/`oci_artifact`, `prefix/expansion`
  vs `checksum/size_bytes`).
- **Content-addressed identity** — the `witcanon:1` contract digest and the
  `content_digest` (sha256, `compose-core::compute_digest` scheme) ducklink just
  adopted are entirely generic; sqlink's `checksum` is the same idea. One shared
  `identity` module.
- **The feedback system** — `lessons-learned.md` + `t-status.py` (the `(T-N …)`
  markers) + `compat-registry.json` (per-crate wasm32-wasip2 status) is identical
  machinery seeded from the same source.
→ **datalink/tooling/**: the engine (scaffold, smoke runner, registry
  read/verify/gen, propagate-wit, content/contract identity, t-status,
  compat-registry schema), parameterized by a per-repo config (the DB name, the
  WIT package, the type map, the templates). Each repo keeps a thin
  `tooling/config` + its `templates/` + its `compat-registry.json` data.

### Tier 2 — host engine (shared crate, more coupling)
The host code is parallel: sqlink `host/src/{compose_provider, prefix_registry,
component_blob_cache, cache, policy, vtab, s3}.rs` ↔ ducklink
`crates/ducklink-{runtime,host}` (`compose_dynlink.rs`, the prefix system, the
compile cache, the network-grant policy, the storage/s3 path). Shared concerns:
- **component loading + capability injection** into a wasmtime `Linker`;
- **`compose:dynlink/linker`** host (resident shared providers) — *both mirror the
  same framework*, `compose_provider.rs` ≈ `compose_dynlink.rs`.
  **STATUS: partially landed → `crates/datalink-dynlink`.** The store-generic
  linker-host machinery (the resolve/invoke resource-table bridge
  `DynLinkBridge`, the generated `compose:dynlink` bindings, `add_to_linker` /
  `imports_linker`) plus a `ProviderBackend` trait now live in
  `datalink-dynlink`, with the `ResidentBackend` (instantiate-once-and-reuse +
  preopens) shipped. **ducklink consumes it** (its `compose_dynlink.rs` is a
  97-line adapter, down from 541; proofs green: dlopen + pylon ml_kmeans +
  smoke 182). **sqlink is DEFERRED**, because its linker host differs on two
  axes the shared sync bridge can't span in one pass:
  1. *async* — sqlink's bindgen is `imports/exports: { default: async }`, so its
     `linker::Host` methods are `async fn`; the shared bridge + `ProviderBackend`
     are sync (matching the framework + ducklink). Unifying needs an *async*
     variant of the bridge/trait (an `async fn` `ProviderBackend` + an
     `add_to_linker` that registers async host fns) — a NEW shared API, not a
     migration.
  2. *resource-table location + trust coupling + dual hosts* — sqlink keeps the
     `instance` table in the **Store** (`HostWrap.resources` / `RunHostWrap`),
     not in the bridge; has TWO host impls (`HostWrap` for the CLI,
     `RunHostWrap` for `.run`); resolves-by-digest inline against a CAS cache +
     `TrustPolicy` (Ed25519 sidecars); and is multi-tenant
     (`TenantedProviders = HashMap<tenant, HashMap<id, ProviderHandle>>`).
  The next slice: add an `async` flavor of `DynLinkBridge`/`ProviderBackend` to
  `datalink-dynlink` (or a `mode: async` cargo feature on the bindgen), then
  land sqlink's `FreshStoreProvider` + `SqliteRuntime` as backend impls, with
  the resource table threaded from the Store via a bridge-accessor that returns
  `Option<&mut ResourceTable>` (sqlink's `self.resources.as_deref_mut()` shape).
- **prefix registry** (the `prefix__name` SPARQL-style dual registration);
- **blob/compile cache** (component-compile-cache);
- **capability policy** (network grants / `policy.rs`);
- **the WIT contract guard** (witcanon digest + `@MAJOR` proxy) — ducklink built
  it; sqlink has the plan; identical design.
→ **datalink/crates/datalink-runtime** (the DB-agnostic engine: load, dispatch,
  prefix, compose:dynlink, cache, policy, contract-guard) generic over a
  `ValueModel`/`TypeMap` trait the DB-specific crate implements (DuckDB rich types
  vs SQLite storage classes). This is the highest-value but highest-effort lift;
  blocked on first unifying wasmtime (ducklink is on 46, sqlink on 45 — see
  Risks).

### Tier 3 — browser runtime (shared plumbing)
sqlink has a real consumer runtime (`browser/src/{host-imports, sqlink-composed,
extension-loader, hash, runtime-bindgen}.js`); ducklink has `web/` harnesses; and
the **compose:dynlink browser host** (jco + `@tegmentum/wasi-polyfill` + JSPI)
already lives in the orchestration framework. **Both depend on the same
`@tegmentum/wasi-polyfill`.** The plumbing — building WASI imports, the
jco-transpiled composed-CLI runtime, the JSPI suspend handling, the
extension-loader/dispatch bridge — is DB-agnostic.
→ **datalink/browser** (or an npm `@tegmentum/datalink-web`): `buildWasiImports`,
  the composed-runtime driver, the extension-loader, the compose:dynlink linker
  shim. Each repo supplies its engine wasm + a thin `Database` facade.

### Tier 4 — docs + CI (templates)
Both ship a Docusaurus `website/` and an identical **`docs-deploy.yml`** (gh-pages
branch push via `peaceiris` — ducklink's was copied from sqlink), plus parallel
`ci.yml` (act-friendly catalog-verify) and fuzz/mutation CI (`fuzz-smoke`/`mutants-
nightly` ↔ `fuzz.sh`/`mutants.sh`).
→ **datalink/templates/**: the Docusaurus theme/scaffold, the `docs-deploy`/`ci`/
  fuzz/mutation workflow templates. Low effort, immediate de-dup.

### Tier 5 — the orchestration-framework adoption (already converging)
Both consume `webassembly-component-orchestration` (now public): sqlink bespoke on
wasmtime 45, ducklink mirrored on 46. The compose:dynlink mirror, the witcanon
identity, and the parked orchestrator-adoption phases are *the same work twice*.
→ Do the framework adoption **once, in datalink** (the shared host engine deps
  `compose-core` for identity/plan/blobs/trust; mirrors the wasmtime-bound linker
  over the shared store), and both engines inherit it.

## Recommended datalink shape

```
datalink/
  crates/
    datalink-runtime/     # Tier 2: load/dispatch/prefix/compose-dynlink/cache/policy/guard,
                          #          generic over a ValueModel (DuckDB vs SQLite types)
    datalink-identity/    # witcanon + content_digest (compose-core scheme)
  tooling/                # Tier 1: scaffold/smoke/registry/verify/gen/propagate/t-status engine
    config.schema.json    #   per-repo config: db name, wit package, type map, template dir
  browser/                # Tier 3: wasi-imports, composed runtime, extension-loader, dynlink shim
  templates/              # Tier 4: docusaurus + docs-deploy/ci/fuzz/mutation workflows
```
ducklink and sqlink each become: the engine wasm + the components + the DB-specific
WIT types + a thin `tooling/config` + `Cargo.toml` deps on the `datalink-*` crates.

## Risks / sequencing

1. **wasmtime version skew is the gate for Tier 2.** ducklink is on 46; sqlink on
   45. The shared `datalink-runtime` (wasmtime-bound) requires both on the same
   wasmtime — bump sqlink 45→46 first (ducklink's 39→46 bump showed the churn is
   small: the `wasmtime::Error`-is-now-distinct fix). **Do Tier 1/4 (pure
   Python/JS/templates, no wasmtime) immediately; gate Tier 2/3 on the sqlink
   bump.**
2. **The ValueModel parameterization is the real design work.** Everything else is
   moving files; the one genuine abstraction is a `ValueModel`/`TypeMap` trait so
   the shared dispatch handles DuckDB rich types and SQLite storage classes
   without forking. Spike this before lifting the host engine.
3. **Migration order:** Tier 1 (tooling) + Tier 4 (templates) now → Tier 5
   (framework adoption once) → Tier 3 (browser) → Tier 2 (host engine, after the
   sqlink wasmtime bump + the ValueModel spike). Each tier is independently
   shippable; consume from `datalink` per-repo as it lands, deleting the mirrored
   copy.

## Quick wins to start (no coupling, no wasmtime)

- Move the **tooling engine** (scaffold/smoke/t-status/registry-verify/gen/identity)
  to `datalink/tooling` + a per-repo config; delete the duplicated scripts.
- Move the **docs-deploy / ci / fuzz / mutants** workflow templates + the
  Docusaurus scaffold to `datalink/templates`.
- Extract the **identity** scheme (witcanon contract digest + content_digest) — the
  one ducklink just finished — as the canonical shared `datalink-identity`, and
  point sqlink's `checksum` at it.
