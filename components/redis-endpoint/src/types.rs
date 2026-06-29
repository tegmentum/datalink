//! CBOR request/response envelope for the `redis-endpoint` provider.
//!
//! A host (or a guest via the dynlink linker) sends a `command` request as a
//! CBOR map and receives the parsed RESP reply back as CBOR. Byte payloads
//! (command arguments, bulk-string replies) ride as CBOR byte strings via
//! `serde_bytes`, not int arrays.

use serde::{Deserialize, Serialize};

/// A `command` request. `addr` is `host:port` (the host assembles it from the
/// extension's connection string before the policy gate, exactly as the
/// http-endpoint provider takes a fully-assembled `url`). `args` is the command
/// and its arguments as raw byte strings (`["SET", "k", "v"]`). Optional
/// `password` triggers an AUTH, optional `db` a SELECT, before the command.
#[derive(Deserialize)]
pub struct CommandRequest {
    pub addr: String,
    pub args: Vec<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub password: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub db: Option<i64>,
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// A parsed RESP2 reply, serialized to CBOR as an externally-tagged enum so the
/// host can reconstruct the typed value:
///   simple/error -> string, int -> i64, bulk -> bytes, array -> list, nil -> unit.
#[derive(Debug, Serialize)]
pub enum RedisReply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(#[serde(with = "serde_bytes")] Vec<u8>),
    Array(Vec<RedisReply>),
    Nil,
}

/// Error surface, mirroring the variant style of the http-endpoint provider.
/// The stable tag is carried in the CBOR error envelope's `context` so a host
/// can reconstruct a typed error.
#[derive(Clone, Debug)]
pub enum RedisError {
    InvalidInput(String),
    TimedOut,
    Connection(String),
    Protocol(String),
    /// A RESP `-ERR ...` reply surfaced as an error (e.g. AUTH/SELECT failure).
    Command(String),
}

impl RedisError {
    pub fn tag(&self) -> &'static str {
        match self {
            RedisError::InvalidInput(_) => "invalid-input",
            RedisError::TimedOut => "timed-out",
            RedisError::Connection(_) => "connection-error",
            RedisError::Protocol(_) => "protocol-error",
            RedisError::Command(_) => "command-error",
        }
    }
}

impl std::fmt::Display for RedisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedisError::InvalidInput(m) => write!(f, "invalid-input: {m}"),
            RedisError::TimedOut => write!(f, "timed-out"),
            RedisError::Connection(m) => write!(f, "connection-error: {m}"),
            RedisError::Protocol(m) => write!(f, "protocol-error: {m}"),
            RedisError::Command(m) => write!(f, "command-error: {m}"),
        }
    }
}
