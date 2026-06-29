// redis-endpoint standalone harness: instantiate the provider component ONCE and
// drive the uniform message endpoint
//
//   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//       -> result<list<u8>, error>
//
// Proves the PILOT thesis (a Redis extension is a TCP wire client, feasible as a
// pure wasm component over wasi:sockets — no native host bridge):
//   1. `manifest`        -> the provider self-describes (name/version/methods).
//   2. unknown method    -> a WIT error variant (no trap).
//   3. empty addr        -> the typed `invalid-input` error (offline, no trap).
//   4. LIVE round-trip   -> PING/SET/GET/INCR/DEL over real TCP via wasi:sockets,
//                           against a Redis server. By default the harness starts
//                           an in-process minimal RESP2 mock on loopback (offline,
//                           deterministic); set REDIS_LIVE_ADDR=host:port to point
//                           at a real redis-server instead.
//   5. warm-once         -> the same resident instance serves every call.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use anyhow::{anyhow, bail, Result};
use ciborium::value::Value;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "dynlink-provider",
    path: "../wit",
});

use exports::compose::dynlink::endpoint::Error as EndpointError;

struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn fmt_err(e: &EndpointError) -> String {
    format!(
        "error(code={:?}, message={:?}, context={:?})",
        e.code, e.message, e.context
    )
}

fn cbor(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

fn txt(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| matches!(k, Value::Text(s) if s == key))
            .map(|(_, val)| val),
        _ => None,
    }
}

fn as_str(v: &Value) -> Option<&str> {
    match v {
        Value::Text(s) => Some(s),
        _ => None,
    }
}

/// Build a `command` CBOR payload: { addr, args: [bytes...] }.
fn command(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    cbor(&Value::Map(vec![
        (txt("addr"), txt(addr)),
        (
            txt("args"),
            Value::Array(args.iter().map(|a| Value::Bytes(a.to_vec())).collect()),
        ),
    ]))
}

/// The reply CBOR is an externally-tagged enum: {"Simple": "..."} / {"Int": n} /
/// {"Bulk": h'..'} / {"Array": [..]} / "Nil". Render it human-readably.
fn render_reply(v: &Value) -> String {
    match v {
        Value::Text(s) if s == "Nil" => "(nil)".into(),
        Value::Map(m) => {
            if let Some((Value::Text(tag), val)) = m.first() {
                match (tag.as_str(), val) {
                    ("Simple", Value::Text(s)) => format!("+{s}"),
                    ("Error", Value::Text(s)) => format!("-{s}"),
                    ("Int", Value::Integer(i)) => format!(":{}", i128::from(*i)),
                    ("Bulk", Value::Bytes(b)) => {
                        format!("\"{}\"", String::from_utf8_lossy(b))
                    }
                    ("Array", Value::Array(items)) => {
                        let parts: Vec<String> = items.iter().map(render_reply).collect();
                        format!("[{}]", parts.join(", "))
                    }
                    _ => format!("{v:?}"),
                }
            } else {
                format!("{v:?}")
            }
        }
        _ => format!("{v:?}"),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let component_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "../target/wasm32-wasip2/release/redis_endpoint.wasm".into());

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &component_path)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let mut builder = WasiCtxBuilder::new();
    builder
        .inherit_stdio()
        .inherit_network()
        .allow_ip_name_lookup(true)
        .allow_tcp(true);
    let wasi = builder.build();

    let host = Host {
        wasi,
        table: ResourceTable::new(),
    };
    let mut store = Store::new(&engine, host);

    // Instantiate ONCE — every call below reuses this resident instance.
    let bindings = DynlinkProvider::instantiate(&mut store, &component, &linker)?;
    let ep = bindings.compose_dynlink_endpoint();

    let mut all_ok = true;

    // --- manifest --------------------------------------------------------
    match ep.call_handle(&mut store, "manifest", &[])? {
        Ok(bytes) => {
            let v: Value = ciborium::de::from_reader(&*bytes)?;
            let name = get(&v, "name").and_then(as_str).unwrap_or("?");
            let version = get(&v, "version").and_then(as_str).unwrap_or("?");
            println!("manifest -> name={name} version={version}");
            if name != "redis-endpoint" {
                all_ok = false;
            }
        }
        Err(e) => {
            println!("manifest -> ERR {}", fmt_err(&e));
            all_ok = false;
        }
    }

    // --- unknown method -> WIT error variant (must NOT trap) -------------
    match ep.call_handle(&mut store, "frobnicate", &[])? {
        Ok(_) => {
            println!("badmeth  -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => println!("badmeth  -> {} (expected, no trap)", fmt_err(&e)),
    }

    // --- empty addr -> typed invalid-input (offline) ---------------------
    match ep.call_handle(&mut store, "command", &command("", &[b"PING"]))? {
        Ok(_) => {
            println!("badaddr  -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => {
            let ok = e.context.as_deref() == Some("invalid-input");
            println!("badaddr  -> {} (expected invalid-input: {ok})", fmt_err(&e));
            if !ok {
                all_ok = false;
            }
        }
    }

    // --- live round-trip over real TCP via wasi:sockets ------------------
    match live_roundtrip(&mut store, &ep) {
        Ok(true) => println!("live     -> PASS (PING/SET/GET/INCR/DEL round-trip over wasi:sockets)"),
        Ok(false) => println!("live     -> FAIL: no replies"),
        Err(e) => {
            println!("live     -> FAIL: {e}");
            all_ok = false;
        }
    }

    println!();
    if all_ok {
        println!("RESULT: PASS — redis-endpoint compose:dynlink/endpoint provider verified \
                  (manifest + typed errors + live RESP round-trip over wasi:sockets + warm-once resident).");
        Ok(())
    } else {
        println!("RESULT: FAIL — see results above.");
        std::process::exit(1);
    }
}

fn call(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
    addr: &str,
    args: &[&[u8]],
) -> Result<Value> {
    let bytes = ep
        .call_handle(store, "command", &command(addr, args))?
        .map_err(|e| anyhow!("{}", fmt_err(&e)))?;
    Ok(ciborium::de::from_reader(&*bytes)?)
}

fn live_roundtrip(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
) -> Result<bool> {
    // Use a real redis if pointed at one; otherwise start the in-process mock.
    let (addr, _guard) = match std::env::var("REDIS_LIVE_ADDR") {
        Ok(a) => {
            println!("       (using live redis at {a})");
            (a, None)
        }
        Err(_) => {
            let addr = start_mock_redis()?;
            println!("       (using in-process RESP2 mock at {addr})");
            (addr, Some(()))
        }
    };

    let r = call(store, ep, &addr, &[b"PING"])?;
    println!("       PING        -> {}", render_reply(&r));
    if render_reply(&r) != "+PONG" {
        bail!("PING expected +PONG");
    }

    let r = call(store, ep, &addr, &[b"SET", b"pilot:key", b"hello-from-wasm"])?;
    println!("       SET k v     -> {}", render_reply(&r));
    if render_reply(&r) != "+OK" {
        bail!("SET expected +OK");
    }

    let r = call(store, ep, &addr, &[b"GET", b"pilot:key"])?;
    println!("       GET k       -> {}", render_reply(&r));
    if render_reply(&r) != "\"hello-from-wasm\"" {
        bail!("GET did not return the value we SET");
    }

    let r = call(store, ep, &addr, &[b"INCR", b"pilot:n"])?;
    println!("       INCR n      -> {}", render_reply(&r));
    if render_reply(&r) != ":1" {
        bail!("INCR expected :1");
    }

    let r = call(store, ep, &addr, &[b"DEL", b"pilot:key"])?;
    println!("       DEL k       -> {}", render_reply(&r));
    if render_reply(&r) != ":1" {
        bail!("DEL expected :1");
    }

    let r = call(store, ep, &addr, &[b"GET", b"pilot:key"])?;
    println!("       GET k (gone)-> {}", render_reply(&r));
    if render_reply(&r) != "(nil)" {
        bail!("GET after DEL expected (nil)");
    }

    Ok(true)
}

/// A minimal in-process RESP2 server good enough to prove the round trip:
/// PING -> +PONG, SET -> +OK (stores), GET -> bulk/nil, INCR -> :n, DEL -> :1/:0.
/// Single-connection-at-a-time, runs on a background thread bound to loopback.
fn start_mock_redis() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    thread::spawn(move || {
        use std::collections::HashMap;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // Each connection (the provider connects per command) gets a fresh
            // store; persist across connections via a process-global map.
            handle_conn(stream);
        }
        fn handle_conn(stream: TcpStream) {
            use std::sync::{Mutex, OnceLock};
            static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
            let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
            let mut w = stream.try_clone().unwrap();
            let mut r = BufReader::new(stream);
            loop {
                let cmd = match read_command(&mut r) {
                    Some(c) if !c.is_empty() => c,
                    _ => return,
                };
                let name = String::from_utf8_lossy(&cmd[0]).to_ascii_uppercase();
                let mut db = store.lock().unwrap();
                let resp: Vec<u8> = match name.as_str() {
                    "PING" => b"+PONG\r\n".to_vec(),
                    "SET" => {
                        let key = String::from_utf8_lossy(&cmd[1]).into_owned();
                        db.insert(key, cmd[2].clone());
                        b"+OK\r\n".to_vec()
                    }
                    "GET" => {
                        let key = String::from_utf8_lossy(&cmd[1]).into_owned();
                        match db.get(&key) {
                            Some(v) => format!("${}\r\n", v.len())
                                .into_bytes()
                                .into_iter()
                                .chain(v.iter().copied())
                                .chain(b"\r\n".iter().copied())
                                .collect(),
                            None => b"$-1\r\n".to_vec(),
                        }
                    }
                    "INCR" => {
                        let key = String::from_utf8_lossy(&cmd[1]).into_owned();
                        let cur: i64 = db
                            .get(&key)
                            .and_then(|v| std::str::from_utf8(v).ok())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        let next = cur + 1;
                        db.insert(key, next.to_string().into_bytes());
                        format!(":{next}\r\n").into_bytes()
                    }
                    "DEL" => {
                        let key = String::from_utf8_lossy(&cmd[1]).into_owned();
                        let existed = db.remove(&key).is_some();
                        format!(":{}\r\n", if existed { 1 } else { 0 }).into_bytes()
                    }
                    _ => b"-ERR unknown command\r\n".to_vec(),
                };
                drop(db);
                if w.write_all(&resp).is_err() {
                    return;
                }
            }
        }
        /// Read one RESP2 array-of-bulk-strings command. Returns None on EOF.
        fn read_command(r: &mut BufReader<TcpStream>) -> Option<Vec<Vec<u8>>> {
            let mut line = String::new();
            if r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            let count: usize = line.strip_prefix('*')?.parse().ok()?;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let mut hdr = String::new();
                r.read_line(&mut hdr).ok()?;
                let len: usize = hdr.trim_end().strip_prefix('$')?.parse().ok()?;
                let mut buf = vec![0u8; len + 2]; // includes trailing CRLF
                r.read_exact(&mut buf).ok()?;
                buf.truncate(len);
                out.push(buf);
            }
            Some(out)
        }
    });
    Ok(addr)
}
