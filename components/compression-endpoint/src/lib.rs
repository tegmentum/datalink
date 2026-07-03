//! `compression-endpoint` — a DB-agnostic `compose:dynlink/endpoint` provider
//! that performs zstd compression/decompression.
//!
//! The host (or a guest, via the dynlink linker) calls the uniform message
//! endpoint:
//!
//!   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//!       -> result<list<u8>, error>
//!
//! `method` selects the op; `payload` is a CBOR-encoded request. The Ok result
//! is the RAW output bytes (the compressed/decompressed blob) — no CBOR wrapping
//! on the response, since it is already `list<u8>`. The op surface mirrors
//! sqlink's `sqlite:extension/compression` WIT contract so a host can route its
//! compression path here as a warm-once resident provider — one libzstd in the
//! catalog, reused by every extension:
//!
//!   * `zstd.compress`        -> CompressReq       -> compressed bytes
//!   * `zstd.decompress`      -> DecompressReq     -> decompressed bytes
//!   * `zstd.compress-dict`   -> CompressDictReq   -> compressed bytes
//!   * `zstd.decompress-dict` -> DecompressDictReq -> decompressed bytes
//!   * `manifest`             -> (CBOR map: name/version/methods)
//!
//! The libzstd wrapper lives in `zstd_ops` (relocated verbatim from the former
//! self-contained `zstd` extension).

wit_bindgen::generate!({
    world: "dynlink-provider",
    path: "wit",
    generate_all,
});

mod zstd_ops;

use exports::compose::dynlink::endpoint::{Error, Guest};
use serde::Deserialize;
use sys::compose::types::ErrorCode;

struct CompressionEndpoint;

#[derive(Deserialize)]
struct CompressReq {
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
    level: i32,
}

#[derive(Deserialize)]
struct DecompressReq {
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
}

#[derive(Deserialize)]
struct CompressDictReq {
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
    #[serde(with = "serde_bytes")]
    dict: Vec<u8>,
    level: i32,
}

#[derive(Deserialize)]
struct DecompressDictReq {
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
    #[serde(with = "serde_bytes")]
    dict: Vec<u8>,
}

fn decode_err(method: &str, e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InvalidInput,
        message: format!("compression-endpoint: cbor decode {method}: {e}"),
        context: Some("invalid-request".to_string()),
    }
}

/// A libzstd op failure (bad frame, dict mismatch, ...).
fn op_err(e: String) -> Error {
    Error {
        code: ErrorCode::ExecTrap,
        message: format!("compression-endpoint: {e}"),
        context: Some("compression-failed".to_string()),
    }
}

fn decode<T: serde::de::DeserializeOwned>(method: &str, payload: &[u8]) -> Result<T, Error> {
    ciborium::de::from_reader(payload).map_err(|e| decode_err(method, e))
}

fn manifest() -> Result<Vec<u8>, Error> {
    use ciborium::value::Value;
    let m = Value::Map(vec![
        (
            Value::Text("name".into()),
            Value::Text("compression-endpoint".into()),
        ),
        (
            Value::Text("version".into()),
            Value::Text(env!("CARGO_PKG_VERSION").into()),
        ),
        (
            Value::Text("methods".into()),
            Value::Array(
                [
                    "manifest",
                    "zstd.compress",
                    "zstd.decompress",
                    "zstd.compress-dict",
                    "zstd.decompress-dict",
                ]
                .iter()
                .map(|s| Value::Text((*s).into()))
                .collect(),
            ),
        ),
    ]);
    let mut out = Vec::new();
    ciborium::ser::into_writer(&m, &mut out).map_err(|e| Error {
        code: ErrorCode::InternalError,
        message: format!("compression-endpoint: cbor encode: {e}"),
        context: Some("internal".to_string()),
    })?;
    Ok(out)
}

impl Guest for CompressionEndpoint {
    fn handle(method: String, payload: Vec<u8>) -> Result<Vec<u8>, Error> {
        match method.as_str() {
            "manifest" => manifest(),
            "zstd.compress" => {
                let r: CompressReq = decode("zstd.compress", &payload)?;
                zstd_ops::compress(&r.data, r.level).map_err(op_err)
            }
            "zstd.decompress" => {
                let r: DecompressReq = decode("zstd.decompress", &payload)?;
                zstd_ops::decompress(&r.data).map_err(op_err)
            }
            "zstd.compress-dict" => {
                let r: CompressDictReq = decode("zstd.compress-dict", &payload)?;
                zstd_ops::compress_dict(&r.data, &r.dict, r.level).map_err(op_err)
            }
            "zstd.decompress-dict" => {
                let r: DecompressDictReq = decode("zstd.decompress-dict", &payload)?;
                zstd_ops::decompress_dict(&r.data, &r.dict).map_err(op_err)
            }
            other => Err(Error {
                code: ErrorCode::NotImplemented,
                message: format!("compression-endpoint: unknown method '{other}'"),
                context: Some("invalid-request".to_string()),
            }),
        }
    }
}

export!(CompressionEndpoint);
