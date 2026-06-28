//! msgpack request/response envelope for the `gdal-endpoint` provider.
//!
//! The wire format is msgpack (rmp-serde). Requests and responses are encoded
//! as msgpack MAPS with named string keys (via `rmp_serde::to_vec_named`), so
//! the shape is self-describing and language-neutral — the deferred
//! `spatialproj-component` rewrite encodes/decodes the very same structs.
//!
//! `transform` request map:
//!   { "wkt": <string>, "from_srid": <int>, "to_srid": <int> }
//! `transform` response map:
//!   { "wkt": <string> }

use serde::{Deserialize, Serialize};

/// `transform` request: reproject `wkt` from EPSG:`from_srid` to EPSG:`to_srid`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransformReq {
    pub wkt: String,
    pub from_srid: i32,
    pub to_srid: i32,
}

/// `transform` response: the reprojected geometry re-emitted as WKT.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransformResp {
    pub wkt: String,
}
