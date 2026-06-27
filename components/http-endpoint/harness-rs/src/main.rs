// http-endpoint standalone harness: instantiate the provider component ONCE and
// drive the uniform message endpoint
//
//   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//       -> result<list<u8>, error>
//
// Proves:
//   1. `manifest`          -> the provider self-describes (name/version/methods).
//   2. invalid-url         -> a bad scheme yields the typed `invalid-url` error
//                             (deterministic, offline; no trap).
//   3. unknown method      -> a WIT error variant (no trap).
//   4. warm-once           -> the same resident instance serves every call.
//
// If `HTTP_LIVE_URL` is set (e.g. a local mock or httpbin), it additionally does
// a live GET and a POST-with-body round-trip against it; otherwise it reports
// that the live round-trip needs an HTTP endpoint.

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

/// Build a `request` CBOR payload.
fn request(method: &str, url: &str, headers: Vec<(&str, &[u8])>, body: Option<&[u8]>) -> Vec<u8> {
    let mut map = vec![
        (txt("method"), txt(method)),
        (txt("url"), txt(url)),
        (
            txt("headers"),
            Value::Array(
                headers
                    .into_iter()
                    .map(|(k, v)| Value::Array(vec![txt(k), Value::Bytes(v.to_vec())]))
                    .collect(),
            ),
        ),
    ];
    if let Some(b) = body {
        map.push((txt("body"), Value::Bytes(b.to_vec())));
    }
    cbor(&Value::Map(map))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let component_path = args.get(1).cloned().unwrap_or_else(|| {
        "../target/wasm32-wasip2/release/http_endpoint.wasm".into()
    });

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
            if name != "http-endpoint" {
                all_ok = false;
            }
        }
        Err(e) => {
            println!("manifest -> ERR {}", fmt_err(&e));
            all_ok = false;
        }
    }

    // --- invalid url (deterministic typed error, offline) ----------------
    match ep.call_handle(&mut store, "request", &request("GET", "ftp://nope/x", vec![], None))? {
        Ok(_) => {
            println!("badurl -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => {
            let ok = e.context.as_deref() == Some("invalid-url");
            println!("badurl -> {} (expected invalid-url: {ok})", fmt_err(&e));
            if !ok {
                all_ok = false;
            }
        }
    }

    // --- unknown method -> WIT error variant (must NOT trap) -------------
    match ep.call_handle(&mut store, "frobnicate", &[])? {
        Ok(_) => {
            println!("bad    -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => println!("bad    -> {} (expected, no trap)", fmt_err(&e)),
    }

    // --- optional live GET + POST ---------------------------------------
    match live_roundtrip(&mut store, &ep) {
        Ok(true) => println!("live   -> PASS (GET + POST round-trip)"),
        Ok(false) => println!(
            "live   -> skipped (set HTTP_LIVE_URL to a base http(s) endpoint, e.g. a local mock, \
             to exercise a real GET + POST; offline checks above proved the contract)"
        ),
        Err(e) => {
            println!("live   -> FAIL: {e}");
            all_ok = false;
        }
    }

    println!();
    if all_ok {
        println!("RESULT: PASS — http-endpoint compose:dynlink/endpoint provider verified (manifest + typed errors + warm-once resident).");
        Ok(())
    } else {
        println!("RESULT: FAIL — see results above.");
        std::process::exit(1);
    }
}

fn live_roundtrip(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
) -> Result<bool> {
    let Ok(base) = std::env::var("HTTP_LIVE_URL") else {
        return Ok(false);
    };
    let base = base.trim_end_matches('/');

    // GET base/get
    let get_url = format!("{base}/get");
    let resp = call(store, ep, &request("GET", &get_url, vec![], None))?;
    let status = get(&resp, "status").and_then(|v| match v {
        Value::Integer(i) => i128::from(*i).try_into().ok(),
        _ => None,
    });
    println!("       GET {get_url} -> status {status:?}");
    if status != Some(200u16) {
        bail!("GET expected 200, got {status:?}");
    }

    // POST base/post with a body
    let post_url = format!("{base}/post");
    let payload = b"hello from http-endpoint provider";
    let resp = call(
        store,
        ep,
        &request(
            "POST",
            &post_url,
            vec![("content-type", b"text/plain")],
            Some(payload),
        ),
    )?;
    let status = get(&resp, "status").and_then(|v| match v {
        Value::Integer(i) => i128::from(*i).try_into().ok(),
        _ => None,
    });
    let body = match get(&resp, "body") {
        Some(Value::Bytes(b)) => b.clone(),
        _ => Vec::new(),
    };
    println!(
        "       POST {post_url} -> status {status:?}, echoed {} body bytes",
        body.len()
    );
    if status != Some(200u16) {
        bail!("POST expected 200, got {status:?}");
    }
    // A mock that echoes the body back should contain our payload.
    if !body.windows(payload.len()).any(|w| w == payload) {
        bail!("POST response did not echo the request body");
    }
    Ok(true)
}

fn call(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
    payload: &[u8],
) -> Result<Value> {
    let bytes = ep
        .call_handle(store, "request", payload)?
        .map_err(|e| anyhow!("{}", fmt_err(&e)))?;
    Ok(ciborium::de::from_reader(&*bytes)?)
}
