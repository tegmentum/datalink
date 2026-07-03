// compression-endpoint standalone harness (Phase-0 spike): instantiate the
// provider component ONCE via wasmtime (the same engine the sqlink host uses)
// and drive the uniform message endpoint:
//
//   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//       -> result<list<u8>, error>
//
// Proves the host<->resident leg end to end with zero sqlink wiring: a zstd
// compress then decompress must round-trip, the frame magic must be 28b52ffd,
// and a dictionary round-trip must recover the input.

use anyhow::{bail, Result};
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

fn compress_req(data: &[u8], level: i64) -> Vec<u8> {
    cbor(&Value::Map(vec![
        (Value::Text("data".into()), Value::Bytes(data.to_vec())),
        (Value::Text("level".into()), Value::Integer(level.into())),
    ]))
}

fn decompress_req(data: &[u8]) -> Vec<u8> {
    cbor(&Value::Map(vec![(
        Value::Text("data".into()),
        Value::Bytes(data.to_vec()),
    )]))
}

fn dict_req(method_data: &[u8], dict: &[u8], level: Option<i64>) -> Vec<u8> {
    let mut m = vec![
        (Value::Text("data".into()), Value::Bytes(method_data.to_vec())),
        (Value::Text("dict".into()), Value::Bytes(dict.to_vec())),
    ];
    if let Some(l) = level {
        m.push((Value::Text("level".into()), Value::Integer(l.into())));
    }
    cbor(&Value::Map(m))
}

fn main() -> Result<()> {
    let component_path = std::env::args().nth(1).unwrap_or_else(|| {
        "../target/wasm32-wasip2/release/compression_endpoint.wasm".into()
    });

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &component_path)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let host = Host {
        wasi: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
    };
    let mut store = Store::new(&engine, host);

    let bindings = DynlinkProvider::instantiate(&mut store, &component, &linker)?;
    let ep = bindings.compose_dynlink_endpoint();

    // 1. manifest self-describe
    match ep.call_handle(&mut store, "manifest", &[])? {
        Ok(bytes) => {
            let v: Value = ciborium::de::from_reader(&bytes[..])?;
            println!("manifest ok: {} bytes", bytes.len());
            let _ = v;
        }
        Err(e) => bail!("manifest failed: {}", fmt_err(&e)),
    }

    // 2. compress -> decompress round-trip
    let payload = b"the quick brown fox jumps over the lazy dog".repeat(16);
    let compressed = match ep.call_handle(&mut store, "zstd.compress", &compress_req(&payload, 3))? {
        Ok(b) => b,
        Err(e) => bail!("compress failed: {}", fmt_err(&e)),
    };
    if compressed.get(..4) != Some(&[0x28, 0xb5, 0x2f, 0xfd]) {
        bail!("bad frame magic: {:02x?}", &compressed.get(..4));
    }
    if compressed.len() >= payload.len() {
        bail!("compression did not shrink redundant payload");
    }
    let restored = match ep.call_handle(&mut store, "zstd.decompress", &decompress_req(&compressed))? {
        Ok(b) => b,
        Err(e) => bail!("decompress failed: {}", fmt_err(&e)),
    };
    if restored != payload {
        bail!("round-trip mismatch: {} vs {} bytes", restored.len(), payload.len());
    }
    println!(
        "round-trip ok: {} -> {} -> {} bytes (magic 28b52ffd)",
        payload.len(),
        compressed.len(),
        restored.len()
    );

    // 3. dictionary round-trip
    let dict = b"http://example.com/api/v1/users/".repeat(8);
    let data = b"http://example.com/api/v1/users/alice".to_vec();
    let dc = match ep.call_handle(&mut store, "zstd.compress-dict", &dict_req(&data, &dict, Some(3)))? {
        Ok(b) => b,
        Err(e) => bail!("compress-dict failed: {}", fmt_err(&e)),
    };
    let dd = match ep.call_handle(&mut store, "zstd.decompress-dict", &dict_req(&dc, &dict, None))? {
        Ok(b) => b,
        Err(e) => bail!("decompress-dict failed: {}", fmt_err(&e)),
    };
    if dd != data {
        bail!("dict round-trip mismatch");
    }
    println!("dict round-trip ok: {} -> {} -> {} bytes", data.len(), dc.len(), dd.len());

    println!("SPIKE PASS: host<->resident zstd works through compose:dynlink/endpoint");
    Ok(())
}
