// gdal-endpoint standalone harness: instantiate the composed resident GDAL
// provider ONCE and drive the uniform message endpoint
//
//   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//       -> result<list<u8>, error>
//
// Proves, with NO ducklink involvement:
//   1. `manifest`   -> the provider self-describes (name/version/methods).
//   2. `transform`  -> reproject WKT 4326->3857, asserted BYTE-FOR-BYTE against
//                      spatialproj's current smoke.expected output, proving the
//                      resident-GDAL endpoint reproduces the build-time-wac path.
//   3. bad SRID     -> Err variant (no trap); the consumer shim maps this to
//                      SQL NULL, matching spatialproj's ST_Transform semantics.
//   4. warm-once    -> the same resident instance serves every call.
//
// The component path defaults to ../gdal-provider.wasm (build.sh output).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    // The POST-composition shape: exports endpoint, GDAL import already satisfied.
    world: "dynlink-provider",
    path: "../wit",
});

use exports::compose::dynlink::endpoint::Error as EndpointError;

#[derive(Serialize)]
struct TransformReq {
    wkt: String,
    from_srid: i32,
    to_srid: i32,
}

#[derive(Deserialize)]
struct TransformResp {
    wkt: String,
}

#[derive(Deserialize)]
struct Manifest {
    name: String,
    version: String,
}

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

// spatialproj-component/smoke.expected, the build-time-wac path we must match.
const EXPECT_WEB_MERCATOR: &str = "POINT(-13627665.271218073 4547675.354340558)";
const EXPECT_ORIGIN: &str = "POINT(0 0)";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let component_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "../gdal-provider.wasm".into());

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &component_path)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio();
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
            let m: Manifest = rmp_serde::from_slice(&bytes)?;
            println!("manifest  -> name={} version={}", m.name, m.version);
            if m.name != "gdal-endpoint" {
                all_ok = false;
            }
        }
        Err(e) => {
            println!("manifest  -> ERR {}", fmt_err(&e));
            all_ok = false;
        }
    }

    // --- transform 4326->3857 (San Francisco) ----------------------------
    all_ok &= transform_case(
        &mut store,
        &ep,
        "POINT(-122.4194 37.7749)",
        4326,
        3857,
        EXPECT_WEB_MERCATOR,
    )?;

    // --- transform 4326->3857 (origin) -----------------------------------
    all_ok &= transform_case(&mut store, &ep, "POINT(0 0)", 4326, 3857, EXPECT_ORIGIN)?;

    // --- bad SRID -> Err (consumer shim maps to NULL) --------------------
    {
        let req = TransformReq {
            wkt: "POINT(-122.4194 37.7749)".into(),
            from_srid: 4326,
            to_srid: 0,
        };
        let payload = rmp_serde::to_vec_named(&req)?;
        match ep.call_handle(&mut store, "transform", &payload)? {
            Ok(_) => {
                println!("bad-srid  -> unexpectedly Ok");
                all_ok = false;
            }
            Err(e) => println!("bad-srid  -> Err (expected -> SQL NULL): {}", fmt_err(&e)),
        }
    }

    // --- unknown method -> Err (no trap) ---------------------------------
    match ep.call_handle(&mut store, "no-such-method", &[])? {
        Ok(_) => {
            println!("bad-meth  -> unexpectedly Ok");
            all_ok = false;
        }
        Err(e) => println!("bad-meth  -> {} (expected, no trap)", fmt_err(&e)),
    }

    println!();
    if all_ok {
        println!("RESULT: PASS — gdal-endpoint resident provider verified (manifest + transform 4326->3857 byte-matching spatialproj's smoke.expected + bad-SRID Err + warm-once resident).");
        Ok(())
    } else {
        println!("RESULT: FAIL — see results above.");
        std::process::exit(1);
    }
}

fn transform_case(
    store: &mut Store<Host>,
    ep: &exports::compose::dynlink::endpoint::Guest,
    wkt: &str,
    from_srid: i32,
    to_srid: i32,
    expected: &str,
) -> Result<bool> {
    let req = TransformReq {
        wkt: wkt.into(),
        from_srid,
        to_srid,
    };
    let payload = rmp_serde::to_vec_named(&req)?;
    let bytes = ep
        .call_handle(store, "transform", &payload)?
        .map_err(|e| anyhow!("transform {wkt} {from_srid}->{to_srid}: {}", fmt_err(&e)))?;
    let resp: TransformResp = rmp_serde::from_slice(&bytes)?;
    let ok = resp.wkt == expected;
    println!(
        "transform -> {wkt} [{from_srid}->{to_srid}] = {} (expected {expected}; match: {ok})",
        resp.wkt
    );
    Ok(ok)
}
