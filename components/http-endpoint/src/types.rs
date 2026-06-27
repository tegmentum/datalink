//! CBOR request/response envelope for the `http-endpoint` provider.
//!
//! These serde structs mirror sqlink's `sqlite:extension/http` WIT contract
//! (request/response records, header `(name, bytes)` fields) so a host can pass
//! its native HTTP parameters through untransformed. The wire format is CBOR
//! (ciborium); byte payloads (header values + bodies) use `serde_bytes` so they
//! ride as CBOR byte strings, not int arrays.

use serde::{Deserialize, Serialize};

/// One header field — name + raw bytes value, matching the WIT
/// `field = tuple<string, list<u8>>`.
pub type Field = (String, serde_bytes::ByteBuf);

/// Outgoing request. `url` is the fully-assembled target (the host builds it
/// from scheme + authority + path-with-query, exactly as the native reqwest
/// path does before the policy gate). `method` is the canonical uppercase verb
/// (GET/POST/…); the host maps the WIT `method` variant to this string.
#[derive(Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<Field>,
    #[serde(default)]
    pub body: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// Incoming response, mirroring the WIT `response` record.
#[derive(Serialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<Field>,
    pub body: serde_bytes::ByteBuf,
}

/// Error surface, mirroring the WIT `http-error` variant. The stable tag is
/// carried in the CBOR error envelope's `context` so a host can reconstruct the
/// typed error.
#[derive(Clone, Debug)]
pub enum HttpError {
    InvalidUrl(String),
    TimedOut,
    ConnectionError(String),
    ProtocolError(String),
    Other(String),
}

impl HttpError {
    pub fn parts(&self) -> (&'static str, Option<String>) {
        match self {
            HttpError::InvalidUrl(m) => ("invalid-url", Some(m.clone())),
            HttpError::TimedOut => ("timed-out", None),
            HttpError::ConnectionError(m) => ("connection-error", Some(m.clone())),
            HttpError::ProtocolError(m) => ("protocol-error", Some(m.clone())),
            HttpError::Other(m) => ("other", Some(m.clone())),
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tag, detail) = self.parts();
        match detail {
            Some(d) => write!(f, "{tag}: {d}"),
            None => write!(f, "{tag}"),
        }
    }
}
