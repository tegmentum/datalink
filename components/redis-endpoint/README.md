# redis-endpoint

A DB-agnostic Redis client provider for the `compose:dynlink/endpoint`
contract. It speaks the Redis **RESP2** wire protocol over **wasi:sockets**
(std `TcpStream`) — the same socket-client path proven by the
`postgres_scanner` / `mysql_scanner` wasm components (libpq / MariaDB over TCP)
and the sibling `http-endpoint` / `s3-endpoint` providers (std `TcpStream`).

This is the **pilot** for the "network client as a pure wasm component" track
(task #207). It demonstrates that a Redis DuckDB extension does **not** need a
native host bridge: a Redis client is just a TCP wire client, and
`wasm32-wasip2` opens TCP sockets today.

## Contract

Exports `compose:dynlink/endpoint@0.1.0`. Dispatch is the uniform message
endpoint with a CBOR request/response envelope:

| method     | request (CBOR)                                         | response (CBOR)                |
| ---------- | ----------------------------------------------------- | ------------------------------ |
| `manifest` | —                                                     | map `{name, version, methods}` |
| `command`  | `{addr, args:[bytes], password?, db?, timeout_ms?}`   | a tagged `RedisReply`          |

`RedisReply` is an externally-tagged enum: `{"Simple": str}` / `{"Error": str}`
/ `{"Int": i64}` / `{"Bulk": bytes}` / `{"Array": [..]}` / `"Nil"`.

Connections are stateless per command (connect → optional AUTH/SELECT → send →
read one reply → close), mirroring the http-endpoint provider's model. This
component does **no** policy gating — a host's per-extension connection policy
check stays host-side, before the provider is invoked.

## Build

```sh
./build.sh   # -> target/wasm32-wasip2/release/redis_endpoint.wasm
```

## Smoke

```sh
cargo test --release            # RESP encode/parse unit tests
cd harness-rs && cargo run --release
```

The harness instantiates the component **once** and drives the endpoint:
manifest, an unknown-method WIT error, an empty-addr typed `invalid-input`
error, then a live **PING / SET / GET / INCR / DEL** round-trip over real TCP
via wasi:sockets. By default it starts an in-process minimal RESP2 mock on
loopback (offline, deterministic); set `REDIS_LIVE_ADDR=host:port` to point it
at a real `redis-server` instead.
