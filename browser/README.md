# @tegmentum/datalink-browser

DB-agnostic browser runtime plumbing for datalink SQL-engine wasm components
(DuckDB, SQLite). This is **Tier 3** of the datalink consolidation
(`CONSOLIDATION.md`): the plumbing shared by every engine facade, lifted from the
ducklink `web/` harnesses and the sqlink `browser/src` runtime.

## What's here

| Module | From | Role |
| --- | --- | --- |
| `polyfill.mjs` | ducklink `run-core.mjs` `configurePolyfill` | the WASI plugin set (cli/io/fs/clocks/random/sockets) on `@tegmentum/wasi-polyfill` + an in-memory FS policy; engine-specific mkdirs / ws-gateway are options |
| `runtime.mjs` | ducklink `run-core.mjs` `instantiateCore` | jco transpile via `createRuntimeBindgen` + JSPI config (`jspiAvailable` feature-detect) |
| `extension-host.mjs` | ducklink `extension-host.mjs` | preload + `coreImports` + callback dispatch, **generalized to a multi-extension router** (per-extension handle namespace) |
| `resolver.mjs` | new (over `registry/index.json`) | the conformance gate (`passed && at === wit_contract`) + content-digest verify + provider precedence |
| `fetch-bytes.mjs` | new | `fetchBytes` / `sha256Hex` helpers |

These are **engine-agnostic**: the DuckDB-specific value/type marshalling, the
TVM spill host, and the exact JSPI import/export lists live in the engine facade
(`@tegmentum/ducklink`) and are passed in as options. SQLite gets the same
plumbing for free (`@tegmentum/sqlink`).

## Status

Built + proven via the `@tegmentum/ducklink` facade in headless Chromium. Worker
harness and the full Tier-3 polish (lifting sqlink's runtime onto this too) are
follow-ons.
