//! `redis-endpoint` — a DB-agnostic `compose:dynlink/endpoint` provider that
//! speaks the Redis RESP2 wire protocol over wasi:sockets.
//!
//! THE PILOT for the "network client as a pure wasm component" track (task
//! #207): the Query.Farm audit classed `redis` as needing a native host bridge,
//! but a Redis extension is a TCP wire *client*, and wasm32-wasip2 opens TCP
//! sockets today (the postgres_scanner / mysql_scanner wasm components prove it
//! for the libpq / MariaDB clients; the s3-endpoint / http-endpoint providers
//! prove it for std `TcpStream`). So no host bridge is needed.
//!
//! The host (or a guest, via the dynlink linker) calls the uniform message
//! endpoint:
//!
//!   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//!       -> result<list<u8>, error>
//!
//! `method` selects the operation; `payload` is a CBOR-encoded request and the
//! Ok result is a CBOR-encoded response:
//!
//!   * `command`  -> CommandRequest  -> RedisReply  (the parsed RESP reply)
//!   * `manifest` -> (CBOR map describing name/version/methods)
//!
//! This component does NO policy gating — a host's per-extension connection
//! policy check stays host-side, BEFORE the provider is invoked, exactly as the
//! sibling http-endpoint / s3-endpoint providers.

wit_bindgen::generate!({
    world: "dynlink-provider",
    path: "wit",
    generate_all,
});

mod resp;
mod types;

use std::time::Duration;

use exports::compose::dynlink::endpoint::{Error, Guest};
use sys::compose::types::ErrorCode;
use types::*;

struct RedisEndpoint;

/// Map a typed `RedisError` to the endpoint error envelope. The stable tag is
/// carried in `context` so a host can reconstruct the typed error.
fn to_error(e: RedisError) -> Error {
    let code = match e {
        RedisError::InvalidInput(_) => ErrorCode::InvalidInput,
        _ => ErrorCode::ExecTrap,
    };
    Error {
        code,
        message: e.to_string(),
        context: Some(e.tag().to_string()),
    }
}

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InvalidInput,
        message: format!("redis-endpoint: cbor decode: {e}"),
        context: Some("invalid-input".to_string()),
    }
}

fn encode_err(e: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InternalError,
        message: format!("redis-endpoint: cbor encode: {e}"),
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
        (Value::Text("name".into()), Value::Text("redis-endpoint".into())),
        (
            Value::Text("version".into()),
            Value::Text(env!("CARGO_PKG_VERSION").into()),
        ),
        (
            Value::Text("methods".into()),
            Value::Array(
                ["manifest", "command"]
                    .iter()
                    .map(|s| Value::Text((*s).into()))
                    .collect(),
            ),
        ),
    ]);
    encode(&m)
}

fn command(payload: &[u8]) -> Result<Vec<u8>, Error> {
    let req: CommandRequest = ciborium::de::from_reader(payload).map_err(decode_err)?;
    if req.addr.is_empty() {
        return Err(to_error(RedisError::InvalidInput("empty addr".into())));
    }
    let args: Vec<Vec<u8>> = req.args.into_iter().map(|b| b.into_vec()).collect();
    let reply = resp::command(
        &req.addr,
        req.password.as_ref().map(|b| b.as_ref()),
        req.db,
        &args,
        req.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
    )
    .map_err(to_error)?;
    encode(&reply)
}

impl Guest for RedisEndpoint {
    fn handle(method: String, payload: Vec<u8>) -> Result<Vec<u8>, Error> {
        match method.as_str() {
            "manifest" => manifest(),
            "command" => command(&payload),
            other => Err(Error {
                code: ErrorCode::NotImplemented,
                message: format!("redis-endpoint: unknown method '{other}'"),
                context: Some("other".to_string()),
            }),
        }
    }
}

export!(RedisEndpoint);
