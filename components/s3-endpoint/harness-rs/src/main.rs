// s3-endpoint standalone harness: instantiate the provider component ONCE and
// drive the uniform message endpoint
//
//   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//       -> result<list<u8>, error>
//
// Proves, with NO live S3 required:
//   1. `manifest`  -> the provider self-describes (name/version/methods).
//   2. `sign`      -> the build+sign (SigV4) + request-construction path is
//                     correct, asserted against AWS's published "GET Object"
//                     SigV4 example vector (deterministic, offline).
//   3. warm-once   -> the same resident instance serves multiple calls.
//
// If `S3_LIVE_URL` (+ creds env) is set, it additionally does a live PUT/GET/
// DELETE round-trip against that endpoint (e.g. a local MinIO); otherwise it
// reports that the live round-trip needs an S3 endpoint.

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

fn endpoint_cfg(url: &str, region: &str, path_style: bool) -> Value {
    Value::Map(vec![
        (txt("url"), txt(url)),
        (txt("region"), txt(region)),
        (txt("path_style"), Value::Bool(path_style)),
    ])
}

fn creds(ak: &str, sk: &str, token: Option<&str>) -> Value {
    Value::Map(vec![
        (txt("access_key_id"), txt(ak)),
        (txt("secret_access_key"), txt(sk)),
        (
            txt("session_token"),
            match token {
                Some(t) => txt(t),
                None => Value::Null,
            },
        ),
    ])
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let component_path = args.get(1).cloned().unwrap_or_else(|| {
        "../target/wasm32-wasip2/release/s3_endpoint.wasm".into()
    });

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &component_path)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    // Provider store WITH outbound network (the capability a host's provider
    // store must supply for the S3 provider to reach the endpoint).
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
            if name != "s3-endpoint" {
                all_ok = false;
            }
        }
        Err(e) => {
            println!("manifest -> ERR {}", fmt_err(&e));
            all_ok = false;
        }
    }

    // --- sign (offline SigV4 proof, AWS doc "GET Object" vector) ----------
    // GET https://examplebucket.s3.amazonaws.com/test.txt, Range bytes=0-9,
    // date 20130524T000000Z, region us-east-1. AWS publishes the expected
    // Signature for these exact inputs.
    const EXPECTED_SIG: &str =
        "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41";
    let sign_req = Value::Map(vec![
        (txt("method"), txt("GET")),
        (txt("endpoint"), endpoint_cfg("https://s3.amazonaws.com", "us-east-1", false)),
        (
            txt("credentials"),
            creds(
                "AKIAIOSFODNN7EXAMPLE",
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                None,
            ),
        ),
        (txt("bucket"), txt("examplebucket")),
        (txt("key"), txt("test.txt")),
        (
            txt("extra_headers"),
            Value::Array(vec![Value::Array(vec![txt("range"), txt("bytes=0-9")])]),
        ),
        (txt("amz_date"), txt("20130524T000000Z")),
    ]);
    match ep.call_handle(&mut store, "sign", &cbor(&sign_req))? {
        Ok(bytes) => {
            let v: Value = ciborium::de::from_reader(&*bytes)?;
            let url = get(&v, "url").and_then(as_str).unwrap_or("?");
            let authz = get(&v, "authorization").and_then(as_str).unwrap_or("");
            println!("sign -> url={url}");
            println!("        authorization={authz}");
            let url_ok = url == "https://examplebucket.s3.amazonaws.com/test.txt";
            let sig_ok = authz.contains(EXPECTED_SIG)
                && authz.contains(
                    "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request",
                )
                && authz.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date");
            println!("        url match: {url_ok}, AWS vector signature match: {sig_ok}");
            if !(url_ok && sig_ok) {
                all_ok = false;
            }
        }
        Err(e) => {
            println!("sign -> ERR {}", fmt_err(&e));
            all_ok = false;
        }
    }

    // --- unknown method -> WIT error variant (must NOT trap) -------------
    match ep.call_handle(&mut store, "no-such-method", &[])? {
        Ok(_) => {
            println!("bad  -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => println!("bad  -> {} (expected, no trap)", fmt_err(&e)),
    }

    // --- optional live round-trip ---------------------------------------
    match live_roundtrip(&mut store, &ep) {
        Ok(true) => println!("live -> PASS (put/get/delete round-trip)"),
        Ok(false) => println!(
            "live -> skipped (set S3_LIVE_URL + S3_LIVE_AK/S3_LIVE_SK [+ S3_LIVE_BUCKET] to \
             exercise a real put/get/delete; signing path is proven offline above)"
        ),
        Err(e) => {
            println!("live -> FAIL: {e}");
            all_ok = false;
        }
    }

    println!();
    if all_ok {
        println!("RESULT: PASS — s3-endpoint compose:dynlink/endpoint provider verified (manifest + offline SigV4 vector + warm-once resident).");
        Ok(())
    } else {
        println!("RESULT: FAIL — see results above.");
        std::process::exit(1);
    }
}

/// Live PUT/GET/DELETE against `S3_LIVE_URL` if configured. Returns Ok(false)
/// when not configured (skip), Ok(true) on a clean round-trip.
fn live_roundtrip(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
) -> Result<bool> {
    let Ok(url) = std::env::var("S3_LIVE_URL") else {
        return Ok(false);
    };
    let region = std::env::var("S3_LIVE_REGION").unwrap_or_else(|_| "us-east-1".into());
    let bucket = std::env::var("S3_LIVE_BUCKET").unwrap_or_else(|_| "test-bucket".into());
    let ak = std::env::var("S3_LIVE_AK").unwrap_or_default();
    let sk = std::env::var("S3_LIVE_SK").unwrap_or_default();
    let path_style = std::env::var("S3_LIVE_PATH_STYLE")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    let endpoint = endpoint_cfg(&url, &region, path_style);
    let cr = creds(&ak, &sk, None);
    let key = "datalink-s3-endpoint-harness.txt";
    let payload = b"hello from s3-endpoint provider".to_vec();

    // PUT
    let put = Value::Map(vec![
        (txt("endpoint"), endpoint.clone()),
        (txt("credentials"), cr.clone()),
        (txt("bucket"), txt(&bucket)),
        (txt("key"), txt(key)),
        (txt("body"), Value::Bytes(payload.clone())),
    ]);
    call_ok(store, ep, "put", &cbor(&put))?;

    // GET
    let getr = Value::Map(vec![
        (txt("endpoint"), endpoint.clone()),
        (txt("credentials"), cr.clone()),
        (txt("bucket"), txt(&bucket)),
        (txt("key"), txt(key)),
    ]);
    let got = call_ok(store, ep, "get", &cbor(&getr))?;
    let v: Value = ciborium::de::from_reader(&*got)?;
    let body = match get(&v, "body") {
        Some(Value::Bytes(b)) => b.clone(),
        _ => bail!("get response missing body bytes"),
    };
    if body != payload {
        bail!("round-trip body mismatch: got {} bytes", body.len());
    }

    // DELETE
    let del = Value::Map(vec![
        (txt("endpoint"), endpoint),
        (txt("credentials"), cr),
        (txt("bucket"), txt(&bucket)),
        (txt("key"), txt(key)),
    ]);
    call_ok(store, ep, "delete", &cbor(&del))?;
    Ok(true)
}

fn call_ok(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
    method: &str,
    payload: &[u8],
) -> Result<Vec<u8>> {
    ep.call_handle(store, method, payload)?
        .map_err(|e| anyhow!("{method}: {}", fmt_err(&e)))
}
