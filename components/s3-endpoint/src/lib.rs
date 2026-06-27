//! `s3-endpoint` — a DB-agnostic `compose:dynlink/endpoint` provider that
//! performs signed S3 object operations over HTTPS.
//!
//! The host (or a guest, via the dynlink linker) calls the uniform message
//! endpoint:
//!
//!   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//!       -> result<list<u8>, error>
//!
//! `method` selects the S3 op; `payload` is a CBOR-encoded request and the Ok
//! result is a CBOR-encoded response. The op surface mirrors sqlink's
//! `sqlite:extension/s3-base` WIT contract so a host can route its native S3
//! path here as a warm-once resident provider:
//!
//!   * `get`    -> GetReq    -> GetResp     (body + metadata)
//!   * `put`    -> PutReq    -> PutResp     (etag)
//!   * `delete` -> DeleteReq -> () (CBOR null)
//!   * `head`   -> HeadReq   -> HeadResp    (metadata)
//!   * `list`   -> ListReq   -> ListResp    (objects + prefixes + pagination)
//!   * `copy`   -> CopyReq   -> PutResp     (etag)
//!   * `sign`   -> SignReq   -> SignResp    (dry-run: build+sign, no network)
//!   * `manifest` -> (CBOR map describing name/version/methods)
//!
//! Policy gating is NOT performed here — it stays host-side, BEFORE the
//! provider is invoked (the host's existing capability check). This component
//! only signs and sends.
//!
//! The S3 signing (`sigv4`) + transport (`s3`, std `TcpStream` + rustls) are
//! reused/adapted from ducklink's `s3fs-component`; the op + parsing surface
//! mirrors sqlink's native `s3.rs`.

wit_bindgen::generate!({
    world: "dynlink-provider",
    path: "wit",
    generate_all,
});

mod s3;
mod sigv4;
mod types;

use exports::compose::dynlink::endpoint::{Error, Guest};
use sys::compose::types::ErrorCode;
use types::*;

struct S3Endpoint;

/// Build the endpoint error envelope from an `S3Error`. The typed S3 error kind
/// is carried in `context` (e.g. `"no-such-key"`) so the host can reconstruct
/// the typed `s3-error` variant; `message` is human-readable.
fn to_error(e: S3Error) -> Error {
    let (tag, _detail) = e.parts();
    let code = match e {
        S3Error::InvalidRequest(_) | S3Error::InvalidBucketName => ErrorCode::InvalidInput,
        S3Error::NetworkError(_) => ErrorCode::ExecTrap,
        _ => ErrorCode::InternalError,
    };
    Error {
        code,
        message: e.to_string(),
        context: Some(tag.to_string()),
    }
}

fn decode_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InvalidInput,
        message: format!("s3-endpoint: cbor decode {what}: {e}"),
        context: Some("invalid-request".to_string()),
    }
}

fn encode_err(e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InternalError,
        message: format!("s3-endpoint: cbor encode: {e}"),
        context: Some("internal".to_string()),
    }
}

fn decode<T: serde::de::DeserializeOwned>(method: &str, payload: &[u8]) -> Result<T, Error> {
    ciborium::de::from_reader(payload).map_err(|e| decode_err(method, e))
}

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).map_err(encode_err)?;
    Ok(out)
}

fn manifest() -> Result<Vec<u8>, Error> {
    use ciborium::value::Value;
    let m = Value::Map(vec![
        (Value::Text("name".into()), Value::Text("s3-endpoint".into())),
        (
            Value::Text("version".into()),
            Value::Text(env!("CARGO_PKG_VERSION").into()),
        ),
        (
            Value::Text("methods".into()),
            Value::Array(
                ["manifest", "get", "put", "delete", "head", "list", "copy", "sign"]
                    .iter()
                    .map(|s| Value::Text((*s).into()))
                    .collect(),
            ),
        ),
    ]);
    encode(&m)
}

impl Guest for S3Endpoint {
    fn handle(method: String, payload: Vec<u8>) -> Result<Vec<u8>, Error> {
        match method.as_str() {
            "manifest" => manifest(),
            "get" => encode(&s3::op_get(decode("get", &payload)?).map_err(to_error)?),
            "put" => encode(&s3::op_put(decode("put", &payload)?).map_err(to_error)?),
            "delete" => {
                s3::op_delete(decode("delete", &payload)?).map_err(to_error)?;
                encode(&ciborium::value::Value::Null)
            }
            "head" => encode(&s3::op_head(decode("head", &payload)?).map_err(to_error)?),
            "list" => encode(&s3::op_list(decode("list", &payload)?).map_err(to_error)?),
            "copy" => encode(&s3::op_copy(decode("copy", &payload)?).map_err(to_error)?),
            "sign" => encode(&s3::op_sign(decode("sign", &payload)?).map_err(to_error)?),
            other => Err(Error {
                code: ErrorCode::NotImplemented,
                message: format!("s3-endpoint: unknown method '{other}'"),
                context: Some("invalid-request".to_string()),
            }),
        }
    }
}

export!(S3Endpoint);
