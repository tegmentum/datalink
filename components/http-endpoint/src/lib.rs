//! `http-endpoint` — a DB-agnostic `compose:dynlink/endpoint` provider that
//! performs plain HTTP/HTTPS requests over wasi:sockets + rustls.
//!
//! The host (or a guest, via the dynlink linker) calls the uniform message
//! endpoint:
//!
//!   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//!       -> result<list<u8>, error>
//!
//! `method` selects the operation; `payload` is a CBOR-encoded request and the
//! Ok result is a CBOR-encoded response. The request/response shape mirrors
//! sqlink's `sqlite:extension/http` host SPI field-for-field so a host can route
//! its native reqwest HTTP path here as a warm-once resident provider:
//!
//!   * `request`  -> HttpRequest  -> HttpResponse  (status + headers + body)
//!   * `manifest` -> (CBOR map describing name/version/methods)
//!
//! `HttpRequest` carries `{ method, url, headers: [(name, bytes)], body?,
//! timeout_ms? }`; the host assembles `url` from scheme + authority +
//! path-with-query exactly as the native path does. This component does NO
//! policy gating — the host's per-extension HTTP policy check stays host-side,
//! BEFORE the provider is invoked. This is the s3-endpoint provider's sibling,
//! reusing its wasi:sockets + rustls transport WITHOUT SigV4 (plain HTTP needs
//! no signing).

wit_bindgen::generate!({
    world: "dynlink-provider",
    path: "wit",
    generate_all,
});

mod http;
mod types;

use exports::compose::dynlink::endpoint::{Error, Guest};
use sys::compose::types::ErrorCode;
use types::*;

struct HttpEndpoint;

/// Build the endpoint error envelope from an `HttpError`. The typed HTTP error
/// kind is carried in `context` (e.g. `"timed-out"`) so the host can
/// reconstruct the typed `http-error`; `message` is human-readable.
fn to_error(e: HttpError) -> Error {
    let (tag, _detail) = e.parts();
    let code = match e {
        HttpError::InvalidUrl(_) => ErrorCode::InvalidInput,
        _ => ErrorCode::ExecTrap,
    };
    Error {
        code,
        message: e.to_string(),
        context: Some(tag.to_string()),
    }
}

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InvalidInput,
        message: format!("http-endpoint: cbor decode: {e}"),
        context: Some("invalid-url".to_string()),
    }
}

fn encode_err(e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InternalError,
        message: format!("http-endpoint: cbor encode: {e}"),
        context: Some("other".to_string()),
    }
}

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).map_err(encode_err)?;
    Ok(out)
}

fn manifest() -> Result<Vec<u8>, Error> {
    use ciborium::value::Value;
    let m = Value::Map(vec![
        (Value::Text("name".into()), Value::Text("http-endpoint".into())),
        (
            Value::Text("version".into()),
            Value::Text(env!("CARGO_PKG_VERSION").into()),
        ),
        (
            Value::Text("methods".into()),
            Value::Array(
                ["manifest", "request"]
                    .iter()
                    .map(|s| Value::Text((*s).into()))
                    .collect(),
            ),
        ),
    ]);
    encode(&m)
}

impl Guest for HttpEndpoint {
    fn handle(method: String, payload: Vec<u8>) -> Result<Vec<u8>, Error> {
        match method.as_str() {
            "manifest" => manifest(),
            "request" => {
                let req: HttpRequest =
                    ciborium::de::from_reader(payload.as_slice()).map_err(decode_err)?;
                encode(&http::execute(req).map_err(to_error)?)
            }
            other => Err(Error {
                code: ErrorCode::NotImplemented,
                message: format!("http-endpoint: unknown method '{other}'"),
                context: Some("other".to_string()),
            }),
        }
    }
}

export!(HttpEndpoint);
