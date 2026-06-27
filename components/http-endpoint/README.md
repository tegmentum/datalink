# http-endpoint

A DB-agnostic `compose:dynlink/endpoint` **provider** component that performs
plain HTTP/HTTPS requests, entirely inside wasm. The sibling of `s3-endpoint`:
it reuses that provider's `wasi:sockets` + rustls TLS transport, **without**
SigV4 signing (plain HTTP needs none).

It is the resident-provider replacement for hosts that carry a native HTTP path
(e.g. sqlink's reqwest-backed `http::Host`): the host warms this provider ONCE
over the `AsyncResidentBackend` and routes every HTTP request through it.

## Contract

Exports the uniform message endpoint:

```
compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
    -> result<list<u8>, error>
```

`method` selects the operation; `payload` is a **CBOR-encoded** request and the
Ok result is a **CBOR-encoded** response. The shape mirrors sqlink's
`sqlite:extension/http` host SPI field-for-field.

| method     | request       | response       | notes                       |
|------------|---------------|----------------|-----------------------------|
| `manifest` | (empty)       | map            | name / version / methods    |
| `request`  | `HttpRequest` | `HttpResponse` | status + headers + body     |

```
HttpRequest  { method: string, url: string,
               headers: [ (name: string, value: bytes) ],
               body: bytes (optional), timeout_ms: u32 (optional) }
HttpResponse { status: u16, headers: [ (name, value: bytes) ], body: bytes }
```

The host assembles `url` from scheme + authority + path-with-query exactly as
its native path does. Header values are CBOR byte strings (matching the WIT
`field = tuple<string, list<u8>>`).

### Policy

This component does **no** policy gating. The host's per-extension HTTP policy
check stays host-side, BEFORE the provider is invoked — the provider only sends
and parses.

## Implementation

* **Transport** — `src/http.rs`, the `s3-endpoint` provider's std `TcpStream` +
  pure-Rust `rustls` transport (URL parse, request write, response parse with
  chunked decoding) minus SigV4. Imports `wasi:sockets` + `wasi:clocks` (via
  std) — **not** `wasi:http` — so the host need only provision standard WASI +
  a network grant on the provider store.
* **Errors** — typed `http-error` (invalid-url / timed-out / connection-error /
  protocol-error / other), carried in the endpoint error envelope's `context`.

## Build

```
./build.sh   # -> target/wasm32-wasip2/release/http_endpoint.wasm
```

## Verify (standalone)

```
cargo test --release                  # url-parse / response-parse / dechunk unit tests
cd harness-rs && cargo run            # manifest + typed errors + warm-once
HTTP_LIVE_URL=http://127.0.0.1:9211 cargo run   # + live GET + POST round-trip
```
