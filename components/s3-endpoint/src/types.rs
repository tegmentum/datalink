//! CBOR request/response envelope for the `s3-endpoint` provider.
//!
//! These serde structs mirror sqlink's `sqlite:extension/s3-base` WIT contract
//! field-for-field (credentials, endpoint, bucket/key, ranges, metadata,
//! options, outputs) so a host can pass its native S3 parameters through
//! untransformed. The wire format is CBOR (ciborium); byte payloads use
//! `serde_bytes` so they ride as CBOR byte strings, not int arrays.

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct S3EndpointConfig {
    pub url: String,
    pub region: String,
    #[serde(default)]
    pub path_style: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct S3Credentials {
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct S3ObjectMetadata {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<u64>,
    pub custom: Vec<(String, String)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct S3ObjectInfo {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<u64>,
    pub storage_class: Option<String>,
}

// ---- requests ----

#[derive(Deserialize)]
pub struct GetReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub range: Option<(u64, u64)>,
    #[serde(default)]
    pub if_match: Option<String>,
    #[serde(default)]
    pub if_none_match: Option<String>,
}

#[derive(Deserialize)]
pub struct PutReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub body: serde_bytes::ByteBuf,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub metadata: Vec<(String, String)>,
}

#[derive(Deserialize)]
pub struct DeleteReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    pub key: String,
}

#[derive(Deserialize)]
pub struct HeadReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    pub key: String,
}

#[derive(Deserialize)]
pub struct ListReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub delimiter: Option<String>,
    #[serde(default)]
    pub max_keys: Option<u32>,
    #[serde(default)]
    pub continuation_token: Option<String>,
}

#[derive(Deserialize)]
pub struct CopyReq {
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub source_bucket: String,
    pub source_key: String,
    pub dest_bucket: String,
    pub dest_key: String,
}

/// Dry-run signing request: build + sign without sending. `amz_date` lets the
/// caller pin a fixed timestamp for deterministic, offline verification against
/// AWS's published SigV4 example vectors.
#[derive(Deserialize)]
pub struct SignReq {
    pub method: String,
    pub endpoint: S3EndpointConfig,
    pub credentials: S3Credentials,
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub query: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub extra_headers: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub body: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub amz_date: Option<String>,
}

// ---- responses ----

#[derive(Serialize)]
pub struct GetResp {
    pub body: serde_bytes::ByteBuf,
    pub metadata: S3ObjectMetadata,
}

#[derive(Serialize)]
pub struct PutResp {
    pub etag: String,
}

#[derive(Serialize)]
pub struct HeadResp {
    pub metadata: S3ObjectMetadata,
}

#[derive(Serialize)]
pub struct ListResp {
    pub objects: Vec<S3ObjectInfo>,
    pub common_prefixes: Vec<String>,
    pub next_continuation_token: Option<String>,
    pub is_truncated: bool,
}

#[derive(Serialize)]
pub struct SignResp {
    pub method: String,
    pub url: String,
    pub host: String,
    pub amz_date: String,
    pub headers: Vec<(String, String)>,
    pub authorization: Option<String>,
}

/// The error surface, mirroring s3-base's `s3-error` variant. Serialized into
/// the CBOR error envelope's `kind` tag so a host can reconstruct the typed
/// error.
#[derive(Clone, Debug)]
pub enum S3Error {
    AccessDenied,
    NoSuchBucket,
    NoSuchKey,
    InvalidBucketName,
    InvalidRequest(String),
    NetworkError(String),
    ParseError(String),
    Internal(String),
}

impl S3Error {
    /// Stable tag + optional detail, used to build the CBOR error envelope.
    pub fn parts(&self) -> (&'static str, Option<String>) {
        match self {
            S3Error::AccessDenied => ("access-denied", None),
            S3Error::NoSuchBucket => ("no-such-bucket", None),
            S3Error::NoSuchKey => ("no-such-key", None),
            S3Error::InvalidBucketName => ("invalid-bucket-name", None),
            S3Error::InvalidRequest(m) => ("invalid-request", Some(m.clone())),
            S3Error::NetworkError(m) => ("network-error", Some(m.clone())),
            S3Error::ParseError(m) => ("parse-error", Some(m.clone())),
            S3Error::Internal(m) => ("internal", Some(m.clone())),
        }
    }
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tag, detail) = self.parts();
        match detail {
            Some(d) => write!(f, "{tag}: {d}"),
            None => write!(f, "{tag}"),
        }
    }
}
