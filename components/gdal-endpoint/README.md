# gdal-endpoint

A DB-agnostic `compose:dynlink/endpoint` **provider** component: a thin byte
endpoint wrapping the typed `gdal:core/srs` interface, so a resident GDAL can
be loaded ONCE and shared via the orchestrator's runtime-DI model ("Guice for
Wasm") — instead of GDAL being `wac`-inlined into each geo extension at build
time.

It is the resident-provider replacement for the legacy
`spatialproj-component`, which still composes GDAL at build time. The
reprojection logic is moved here verbatim.

## Why an endpoint wrapper

The orchestrator's resident model is **byte-endpoint only**: a provider exports
`compose:dynlink/endpoint.handle(method, payload) -> result<list<u8>, error>`;
there is no typed dynamic-resolve. GDAL exports the *typed* `gdal:core/srs`, so
it needs this thin wrapper to become a resident provider (loaded once, shared —
like `pylon`).

## Contract

Exports the uniform message endpoint:

```
compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
    -> result<list<u8>, error>
```

`method` selects the operation; `payload` is a **msgpack-encoded** request and
the Ok result is a **msgpack-encoded** response.

| method      | request        | response        | notes                       |
|-------------|----------------|-----------------|-----------------------------|
| `manifest`  | (empty)        | map             | name / version / methods    |
| `transform` | `TransformReq` | `TransformResp` | reproject WKT between EPSG   |

### Wire shape (msgpack)

Requests/responses are msgpack **maps with named string keys** (encoded via
`rmp_serde::to_vec_named`), so the shape is self-describing and language-neutral.

`transform` request:

```
{ "wkt": <string>, "from_srid": <int32>, "to_srid": <int32> }
```

`transform` response:

```
{ "wkt": <string> }
```

`manifest` response:

```
{ "name": <string>, "version": <string>, "methods": [<string>...] }
```

### Error / NULL semantics

spatialproj's `ST_Transform` returns SQL **NULL** on any failure (`from_srid`
or `to_srid` <= 0, unparseable WKT, transform error). Here that surfaces as the
WIT result's **Err** variant (`context = "transform-failed"`). The consumer
shim maps any Err from `invoke` back to NULL, preserving the original behaviour
exactly. Unknown methods return `not-implemented` (no trap).

### Policy

This component does **no** policy gating — the host's capability check stays
host-side, BEFORE the provider is invoked.

## Implementation

* **Reprojection** — `src/lib.rs`, moved verbatim from ducklink's
  `spatialproj-component`: `SpatialRef::from_epsg` (traditional GIS axis order)
  + a coordinate `Transform` + a `wkt`/`geo-types` coordinate walk, re-emitted
  as WKT. EPSG codes resolve against PROJ's `proj.db`, embedded in the gdal
  component (no host filesystem needed).
* **Typed dependency** — imports `gdal:core/srs@0.1.0` (pre-composition world
  `gdal-provider`), satisfied ONCE by composition (below).
* **Envelope** — msgpack (`rmp-serde`).

## Build (compose the resident provider)

```
./build.sh   # -> gdal-provider.wasm  (~27.9 MB; GDAL/PROJ + proj.db bundled once)
```

`build.sh` builds `gdal-endpoint` to `wasm32-wasip2`, then `wac plug`s it ONCE
with `~/git/gdal-wasm/build/bin/gdal.component.wasm` (overridable via
`GDAL_COMPONENT`) to satisfy the `gdal:core/srs` import. The result exports only
`compose:dynlink/endpoint` and imports only WASI — the host loads it once and
shares it as a resident.

> The prebuilt GDAL component uses a few digit-leading WIT label segments
> (`get-extent-3d`, `promote-to-3d`, …) that some pinned wasmtime builds reject;
> `build.sh` renames them to a same-length kebab-valid form (`-3d`->`-d3`,
> `-2d`->`-d2`) in the GDAL binary before composing (the same hack
> `spatialproj-component/compose.sh` uses). `gdal-endpoint` calls none of them.

## Verify (standalone, no ducklink)

```
cd harness-rs && cargo run --release          # defaults to ../gdal-provider.wasm
```

The harness instantiates the composed provider ONCE and drives
`endpoint.handle`: `manifest`, two `transform` cases asserted **byte-for-byte**
against `spatialproj-component/smoke.expected`
(`POINT(-122.4194 37.7749)` 4326->3857 = `POINT(-13627665.271218073
4547675.354340558)`; `POINT(0 0)` 4326->3857 = `POINT(0 0)`), a bad-SRID case
that returns Err (→ SQL NULL), and an unknown-method error path — proving the
resident-GDAL endpoint reproduces the build-time-wac result before any consumer
migration.
