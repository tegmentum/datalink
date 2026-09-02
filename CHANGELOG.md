# Changelog

All notable changes to the datalink workspace crates.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [SemVer](https://semver.org/) and is pre-1.0, so
0.y.z minor bumps may break API.

## [Unreleased]

## [0.1.0] — 2026-09-02

First tagged release. Establishes a stable pin for downstream repos
(ducklink, sqlink, ducklink-extension, datafission,
sqlink-shim-codegen, sqlite-wit) that previously pinned datalink at
individual git revs.

Consumers can now pin to a tag instead of a git rev:

```toml
datalink-contract        = { git = "https://github.com/tegmentum/datalink.git", tag = "v0.1.0" }
datalink-dynlink         = { git = "https://github.com/tegmentum/datalink.git", tag = "v0.1.0" }
datalink-dynlink-wasmos  = { git = "https://github.com/tegmentum/datalink.git", tag = "v0.1.0" }
# and every other workspace crate listed below
```

### Workspace crates in the v0.1.0 shape

- `datalink-contract` — shared runtime contract-version load guard
  (lifted from ducklink/sqlink so both hosts share one guard).
- `datalink-dynlink` — the store-generic `compose:dynlink/linker`
  host machinery + `ResidentBackend` (instantiate-once-and-reuse
  provider lifecycle). Both wasm-component hosts share this.
- `datalink-dynlink-wasmos` — the ADR-0029-abstraction sibling of
  `datalink-dynlink`. Same WIT world, same resident-backend +
  provider-registry semantics, but no direct wasmtime dependency
  (consumes `SelectedRuntime` + `HostImports` + `Value` from
  `wasmos-runtime-api`). Coexists with `datalink-dynlink` on this
  release; consumers migrate piecewise per ADR-0029 Phase 6.2.
- `datalink-policy` — canonical capability-grant types (network
  grants, per-name allow/deny lists).
- `datalink-prefix` — extension-name prefix vocabulary (retired
  in favour of the extension v5.0.0 `prefix.name` model, but the
  crate stays for legacy callers).
- `datalink-valuemodel` — the shared `Value` model used across
  extension-facing surfaces.
- `datalink-extcore` — the extension-core substrate.
- `datalink-shim-codegen-core` — shared codegen substrate for the
  four shim-bridge emitters below.
- `datalink-shim-sqlite-emit` — legacy SQLite shim emitter.
- `datalink-shim-sqlite-dynlink-emit` — dynlink SQLite shim emitter.
- `datalink-shim-duckdb-emit` — legacy DuckDB shim emitter.
- `datalink-shim-duckdb-dynlink-emit` — dynlink DuckDB shim emitter.
- `datalink-shim-datafission-emit` — datafission emitter.

### Reference material

- Consumer repos that previously pinned by rev are documented in
  the wasmos runtime-abstraction docs (see wasmos `docs/design/
  runtime-abstraction/`); the datalink release note at the wasmos
  side lists which rev each consumer used to hold.

[Unreleased]: https://github.com/tegmentum/datalink/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/tegmentum/datalink/releases/tag/v0.1.0
