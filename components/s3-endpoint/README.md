# s3-endpoint

A DB-agnostic `compose:dynlink/endpoint` **provider** component that performs
signed (AWS SigV4) S3 object operations over HTTPS, entirely inside wasm.

It is the resident-provider replacement for hosts that carry a native S3 path
(e.g. sqlink's `host/src/s3.rs`): the host warms this provider ONCE over the
`AsyncResidentBackend` and routes every S3 call through it.

## Contract

Exports the uniform message endpoint:

```
compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
    -> result<list<u8>, error>
```

`method` selects the operation; `payload` is a **CBOR-encoded** request and the
Ok result is a **CBOR-encoded** response. The op surface mirrors sqlink's
`sqlite:extension/s3-base` WIT contract field-for-field, so a host can pass its
native S3 parameters through untransformed.

| method     | request    | response   | notes                          |
|------------|------------|------------|--------------------------------|
| `manifest` | (empty)    | map        | name / version / methods       |
| `get`      | `GetReq`   | `GetResp`  | body (CBOR bytes) + metadata   |
| `put`      | `PutReq`   | `PutResp`  | etag                           |
| `delete`   | `DeleteReq`| null       |                                |
| `head`     | `HeadReq`  | `HeadResp` | metadata only                  |
| `list`     | `ListReq`  | `ListResp` | objects + prefixes + paging    |
| `copy`     | `CopyReq`  | `PutResp`  | server-side copy               |
| `sign`     | `SignReq`  | `SignResp` | dry-run: build + sign, no I/O  |

Request/response field shapes are in [`src/types.rs`](src/types.rs). Byte
payloads ride as CBOR byte strings (`serde_bytes`).

`endpoint` carries `{ url, region, path_style }`; `credentials` carries
`{ access_key_id, secret_access_key, session_token? }`. Empty credentials =
anonymous (public-bucket) mode.

### Policy

This component does **no** policy gating. The host's capability check stays
host-side, BEFORE the provider is invoked — the provider only signs and sends.

## Implementation

* **Signing** — `src/sigv4.rs`, reused verbatim from ducklink's
  `s3fs-component` (hand-rolled SigV4 on `hmac` + `sha2`; verified offline
  against AWS's published "GET Object" example vector).
* **Transport** — `src/s3.rs`, a generalized version of s3fs's `https_get`
  (std `TcpStream` + pure-Rust `rustls` TLS) that handles any method, request
  headers, and a request body. Imports `wasi:sockets` + `wasi:clocks` (via std)
  — **not** `wasi:http` — so the host need only provision standard WASI + a
  network grant on the provider store.
* **Ops / parsing** — op surface + XML/metadata parsing mirror sqlink's native
  `s3.rs`.

## Build

```
./build.sh   # -> target/wasm32-wasip2/release/s3_endpoint.wasm
```

## Verify (standalone, no live S3)

```
cargo test --release                 # sigv4 vector + url/xml/date unit tests
cd harness-rs && cargo run           # manifest + offline SigV4 vector + warm-once
```

The harness instantiates the component once and drives `endpoint.handle`:
`manifest`, a `sign` dry-run asserted against the AWS SigV4 example vector, and
an unknown-method error path. Set `S3_LIVE_URL` (+ `S3_LIVE_AK`/`S3_LIVE_SK`/
`S3_LIVE_BUCKET`/`S3_LIVE_PATH_STYLE`) to add a live PUT/GET/DELETE round-trip
against a real endpoint (e.g. MinIO).
