# Networked Query.Farm extensions: socket-clients vs. the native host-bridge

Task #207. Designs the native host-bridge track for the Query.Farm extensions
that genuinely cannot run as a pure wasm component, **after** re-examining the
original audit — which turns out to be too pessimistic. Includes the
re-classification, the host-bridge architecture for the cases that truly need
it, a phased plan, and the result of the pilot (`components/redis-endpoint/`).

## 0. The eight (from the Query.Farm audit)

`airport` (Arrow Flight gRPC), `kafka`/`tributary` (Kafka topics), `redis`
(Redis client), `httpserver` (HTTP server in DuckDB), `radio`
(WebSocket / Redis pub-sub), `adbc_scanner` (ADBC drivers), `shellfs` (shell
commands as files), `quack-server` (DuckDB client-server protocol).

The audit classed all eight as "needs networked server / persistent sockets /
native drivers / host shell" and therefore as host-bridge work. That conflates
two very different things: **opening an outbound socket** (which wasm already
does) with **owning an inbound listener / a native driver / a host shell**
(which it genuinely cannot).

## 1. Re-classification — the audit was too pessimistic

We already run real TCP+TLS clients **from inside wasm32-wasip2**:

- `postgres_scanner` connects to PostgreSQL over TCP (libpq cross-compiled to
  wasi, the wasi:sockets graft) — verified scans, pushdown, 1000-row results.
- `mysql_scanner` connects to MariaDB **over TLS** (MariaDB Connector/C +
  openssl on wasi).
- `httpfs` fetches `https://` over wasi:sockets with cert verification on.
- `s3-endpoint` / `http-endpoint` providers do S3 / HTTP over std `TcpStream` +
  rustls, all wasi:sockets.
- `quack` **client** rides DuckDB's `HTTPUtil` (the curl path) to a remote
  quack server — already shipped.

So the discriminator is **direction and dependency**, not "networking":

- An **outbound wire client** (connect → request → reply) is feasible as a pure
  wasm component. wasi:sockets gives `connect`; that is all a protocol client
  needs.
- An **inbound listener** (`listen`/`accept`), a **native driver** dlopen'd
  host-side, or a **host shell** (process spawn) cannot live in the wasm
  sandbox and genuinely needs the native host-bridge.

| Extension       | Audit said            | Reality                                                                    | Class |
| --------------- | --------------------- | -------------------------------------------------------------------------- | ----- |
| **redis**       | native host bridge    | RESP2 over TCP — an outbound wire **client**                               | **(a) socket-client** — PILOTED |
| **quack** (client) | needs server       | rides `HTTPUtil`/curl to a remote server                                   | **(a)** — already shipped |
| **kafka/tributary** | Kafka topics      | Kafka wire protocol over TCP — an outbound **client** (producer/consumer)  | **(a)** — feasible, heavy |
| **airport**     | Arrow Flight gRPC     | Flight = gRPC/HTTP2 **client** to a remote Flight server                   | **(a)** — feasible, caveated |
| **httpserver**  | HTTP server in DuckDB | inbound `listen`/`accept` — a real server                                  | **(b) host-bridge** |
| **radio**       | WS / Redis pub-sub    | inbound WS server + persistent async push (outbound pub/sub is (a))        | **(b)** (mixed) |
| **quack-server**| server                | inbound listener — DuckDB client-server                                    | **(b)** — already shipped (host-bridge) |
| **shellfs**     | shell commands        | host process spawn / pipe                                                  | **(b)** — host shell |
| **adbc_scanner**| ADBC drivers          | dlopen native ADBC driver `.so` host-side                                  | **(b)** — native driver |

### (a) Socket-clients — pure wasm components, no host bridge

`redis`, `kafka/tributary`, `airport`, plus the already-shipped `quack` client.
These need only `wasi:sockets` `connect`, exactly like `postgres_scanner`.
Build them the way we build the connectors and the endpoint providers — a wasm
component that owns the wire protocol — **not** a host bridge.

Caveats by difficulty:

- **redis** — trivial. RESP2 is a tiny text/length-prefixed protocol over plain
  TCP. **Piloted here** (`components/redis-endpoint/`).
- **kafka/tributary** — feasible but a real project. The Kafka protocol is
  large (metadata, partition leadership, consumer groups, offset commits,
  long-lived connections, usually SASL+TLS). Either cross-compile `librdkafka`
  to wasi (the libpq/MariaDB recipe) or use a pure-Rust client. The *transport*
  is not the blocker; the *protocol surface* is the work.
- **airport** — feasible with a caveat. Arrow Flight is gRPC over HTTP/2; the
  httpfs curl build already carries `nghttp2`, so an HTTP/2 client transport
  exists. The hard part is gRPC **streaming** (`DoGet` is server-streaming):
  Flight's value is streaming Arrow batches, and a one-shot request/reply
  endpoint does not express that well. A first cut can do unary Flight RPCs
  (handshake, `GetFlightInfo`) over HTTP/2; streaming `DoGet`/`DoPut` needs a
  streaming endpoint surface (see §2, async push). Classed (a) because it is a
  client, but it is the heaviest of the four.

### (b) True host-bridge — server / native-driver / host-shell

`httpserver`, `radio` (server side), `quack-server` (shipped), `shellfs`,
`adbc_scanner`. These step outside the sandbox and need a native host process to
own the socket / driver / shell and bridge bytes to the component.

## 2. Host-bridge architecture for the (b) set

Do **not** bump the frozen `duckdb:extension` contract (`@4.0.0`). Every bridge
below rides an **existing, opt-in** surface, so un-importing components never
rebuild and `types`/`runtime` enums are never touched (the only thing that would
force a MAJOR). Two shapes cover all of (b), and both are already proven:

### Shape A — host-owns-socket *inbound* bridge (servers)

For `httpserver`, `radio`-WS, `quack-server`: the **native host** owns the
`TcpListener` + single-threaded accept loop and frames each inbound request; the
**component** implements a `handle-request` export that does the protocol/handler
logic. The component never holds the socket — it only receives a request body
and returns a response body.

This is exactly the `quack-server` pattern already shipped in ducklink
(`crates/ducklink-host/src/quack_server.rs`, modeled on `ui_server.rs`): a
listen-less `handle`-style export + a host accept loop framing the wire protocol.
It is an **additive opt-in export world** off `@4.0.0` (the precedent is the
existing `parser-host` interface + the quack `handle-quack-request` export) — not
a types/runtime change. The reusable piece to extract is a generic
`host-listen` bridge: `(bind addr, accept loop) → component.handle-request(bytes)
→ write reply`, parameterized by the wire framing (HTTP/1.1, WebSocket upgrade,
quack-over-HTTP).

### Shape B — host-resident *capability provider* via compose:dynlink (drivers, shell, outbound helpers)

For `adbc_scanner` (native driver) and `shellfs` (host shell): expose the
host-mediated capability as a **`compose:dynlink/endpoint` resident provider** —
the same mechanism as `s3-endpoint`, `gdal-endpoint`, and the pylon ML provider.
The component resolves it through the `compose:dynlink/linker` import (the
dual-import world precedent: `mlkmeans` imports **both** `duckdb:extension` and
`compose:dynlink/linker@0.1.0`). CBOR request/response. This keeps the
`duckdb:extension` contract frozen — the bridge rides `compose:dynlink/linker
@0.1.0`, which is additive and opt-in (only components that import it get it).

- `shellfs` → a host-side `shell-endpoint` provider: `exec(argv, stdin) ->
  {stdout, stderr, exit}` (and a streaming variant). The provider runs the
  process host-side and returns bytes.
- `adbc_scanner` → a host-side `adbc-endpoint` provider: the host owns the
  dlopen'd ADBC driver and exposes `connect` / `statement-execute → arrow
  stream`. Warm-once resident (the s3/pylon model). Where the ADBC target is
  itself a known wire protocol (PostgreSQL ADBC = libpq, FlightSQL ADBC = Arrow
  Flight) the cleaner route is to **sidestep ADBC entirely** and use the (a)
  socket-client for that backend.

### Async push (radio, Flight streaming)

Servers and streaming clients need to deliver rows to DuckDB **asynchronously**,
not as a single reply. The host bridge owns the long-lived
connection/subscription and feeds a host→component (or host→DuckDB) delivery
channel: the accept loop (Shape A) or a resident provider (Shape B) pushes
batches as they arrive, and the extension surfaces them through a streaming
table function. This is the one capability neither shape provides out of the
box today; it is additive (a new streaming export/import world off `@4.0.0`) and
is the gating work for `radio`'s push side and Flight `DoGet`.

### Trust / sandbox boundary

Anything in (b) runs **native code host-side, outside the wasm sandbox**:
binding a listen socket, dlopen'ing a driver, spawning a shell. Each is
**default-deny** and gated by host policy upstream of the bridge (the same place
the s3/http providers' policy check already sits — *before* invoke):

- **listen sockets** — explicit CLI opt-in + bind-addr allow-list (the
  `ducklink quack-serve --port/--token` precedent).
- **native driver dlopen** — signed / attested + an `allow_native_providers`
  switch; never load an unsigned driver by default.
- **shell exec** — the sharpest edge: default-deny, an explicit command
  allow-list, no shell interpolation. Off unless the operator opts in.

The component still only ever sends/receives **bytes** across the boundary; it
never holds the socket, driver handle, or child process (the same property that
makes `compose:dynlink` resolve safe — the guest holds a handle, not the
provider's memory).

### Lifecycle

- **Inbound servers (Shape A)** — host owns the accept-loop lifetime via a CLI
  subcommand (`quack-serve` precedent); the component handler is stateless
  per-request or holds a host-provided DB connection.
- **Resident providers (Shape B)** — warm-once: instantiated lazily on first
  resolve into a resident store and shared across calls (the s3/pylon model;
  outbound connections may be pooled inside the provider).

## 3. Phased plan for the (b) set

| Phase | Item | Shape | Status / notes |
| ----- | ---- | ----- | -------------- |
| 0 | `quack-server`, `ui_server` | A (inbound listen) | **DONE** — proves the host-owns-socket bridge end-to-end |
| 1 | `redis` socket-client | (a), not (b) | **DONE (this task)** — proves the client-over-sockets path cheaply |
| 2 | `shellfs` → `shell-endpoint` provider | B | lowest-complexity true bridge (no wire protocol); default-deny + command allow-list |
| 3 | `httpserver` | A | generalize the quack accept loop into a reusable `host-listen` HTTP/1.1 bridge; component exports `handle-request` |
| 4 | `radio` | A + (a) + async-push | inbound WS-listen (Phase 3 + WS upgrade/framing) + outbound Redis pub/sub (rides the Phase-1 redis client); needs the async-push channel |
| 5 | `adbc_scanner` → `adbc-endpoint` provider | B | host-side native driver dlopen; trust-gated; or sidestep known targets to the (a) clients |

The (a) socket-clients (`kafka/tributary`, `airport`) are a separate,
parallel track — each is a wasm component owning its wire protocol (the
`postgres_scanner` recipe), no host bridge, sequenced by protocol weight
(kafka < airport).

## 4. Pilot result — `components/redis-endpoint/`

Built the highest-value tractable case as a **real wasm component** and smoked
it end-to-end.

- **What it is** — a `compose:dynlink/endpoint@0.1.0` provider speaking the
  Redis **RESP2** wire protocol over `wasi:sockets` (std `TcpStream`, no clone —
  wasi sockets are driven through shared `&TcpStream` references). Methods:
  `manifest`, `command{addr, args, password?, db?, timeout_ms?}`. CBOR envelope,
  same shape as the sibling `http-endpoint`. No policy gating in the component.
- **Artifact** — `target/wasm32-wasip2/release/redis_endpoint.wasm` (~244 KB);
  `wasm-tools` confirms `import wasi:sockets/tcp@0.2.6` and `export
  compose:dynlink/endpoint@0.1.0`.
- **Smoke** — a wasmtime harness (wasmtime-wasi 46.0.1) instantiates the
  component **once** and drives it. The live round-trip runs over **real TCP via
  wasi:sockets** against an in-process minimal RESP2 server on loopback (offline
  + deterministic; `REDIS_LIVE_ADDR=host:port` points it at a real
  `redis-server`). The same wasi:sockets transport is what `postgres_scanner` /
  `mysql_scanner` use against live servers.

```
manifest -> name=redis-endpoint version=0.1.0
badmeth  -> error(code=ErrorCode::NotImplemented, ... 'frobnicate') (expected, no trap)
badaddr  -> error(... context=Some("invalid-input")) (expected invalid-input: true)
       (using in-process RESP2 mock at 127.0.0.1:51380)
       PING        -> +PONG
       SET k v     -> +OK
       GET k       -> "hello-from-wasm"
       INCR n      -> :1
       DEL k       -> :1
       GET k (gone)-> (nil)
live     -> PASS (PING/SET/GET/INCR/DEL round-trip over wasi:sockets)

RESULT: PASS — redis-endpoint compose:dynlink/endpoint provider verified
(manifest + typed errors + live RESP round-trip over wasi:sockets + warm-once resident).
```

Plus 7 RESP encode/parse unit tests (`cargo test`).

This proves the (a) thesis: a Redis DuckDB extension is a TCP wire client and
runs as a pure wasm component — **no host bridge** — exactly the path the audit
wrote off. The same recipe carries the rest of (a).

## 5. Honest verdict — feasible vs. genuinely infeasible

- **Wrongly written off by the audit (feasible as pure wasm clients):**
  `redis` (proven here), `quack` client (shipped), `kafka/tributary` (feasible,
  protocol-heavy), `airport` (feasible for unary RPCs; streaming `DoGet` needs
  the async-push surface).
- **Genuinely needs the host-bridge:** `httpserver`, `quack-server` (shipped),
  `radio` (server + async push), `shellfs` (host shell), `adbc_scanner`
  (native driver). All are tractable by precedent (Shapes A/B both shipped
  once) — none is a research blocker — and all fit additively off the frozen
  `@4.0.0` contract.
- **The one missing primitive:** an **async-push / streaming** delivery channel
  (host→component) for `radio`'s subscriptions and Flight streaming. Additive,
  but real work; it gates the streaming features specifically (not the
  request/reply bridges).
- **`shellfs` is the trust outlier:** technically the simplest bridge, but
  arbitrary host command execution is the sharpest sandbox-escape edge — ship it
  default-deny behind an explicit allow-list, or not at all.
