//! `gdal-endpoint` — a DB-agnostic `compose:dynlink/endpoint` provider that
//! wraps the typed `gdal:core/srs` interface as the uniform byte endpoint.
//!
//! The host (or a guest, via the dynlink linker) calls:
//!
//!   compose:dynlink/endpoint.handle(method: string, payload: list<u8>)
//!       -> result<list<u8>, error>
//!
//! `method` selects the op; `payload` is a msgpack-encoded request and the Ok
//! result is a msgpack-encoded response:
//!
//!   * `manifest`  -> (empty)       -> map { name, version, methods }
//!   * `transform` -> TransformReq  -> TransformResp   { wkt }
//!
//! `transform` reprojects a WKT geometry from EPSG:`from_srid` to
//! EPSG:`to_srid` by driving the COMPOSED GDAL component: `SpatialRef::from_epsg`
//! + a coordinate `Transform` + a geo/wkt coordinate walk. EPSG codes are
//! resolved by PROJ's proj.db, embedded inside the gdal component (no host
//! filesystem needed). The reprojection logic is moved verbatim from
//! ducklink's `spatialproj-component`.
//!
//! ERROR/NULL SEMANTICS: spatialproj's `ST_Transform` yields SQL NULL on any
//! failure (bad SRID, unparseable WKT, transform error). Here that surfaces as
//! the WIT result's Err variant; the consumer shim maps any Err from `invoke`
//! back to NULL, preserving the original behaviour exactly.

wit_bindgen::generate!({
    world: "gdal-provider",
    path: "wit",
    generate_all,
});

mod types;

use exports::compose::dynlink::endpoint::{Error, Guest};
use sys::compose::types::ErrorCode;
use types::{TransformReq, TransformResp};

// The composed GDAL dependency (typed `gdal:core/srs`).
use gdal::core::srs;

struct GdalEndpoint;

fn err(code: ErrorCode, message: String, context: &str) -> Error {
    Error {
        code,
        message,
        context: Some(context.to_string()),
    }
}

fn decode_err(e: impl std::fmt::Display) -> Error {
    err(
        ErrorCode::InvalidInput,
        format!("gdal-endpoint: msgpack decode: {e}"),
        "invalid-request",
    )
}

fn encode_err(e: impl std::fmt::Display) -> Error {
    err(
        ErrorCode::InternalError,
        format!("gdal-endpoint: msgpack encode: {e}"),
        "internal",
    )
}

fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, Error> {
    rmp_serde::from_slice(payload).map_err(decode_err)
}

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, Error> {
    rmp_serde::to_vec_named(v).map_err(encode_err)
}

/// Traditional GIS axis order (lon, lat) so WKT x=lon / y=lat flows straight
/// through PROJ. (GDAL's OAMS_TRADITIONAL_GIS_ORDER == 0.)
const TRADITIONAL_GIS_ORDER: u32 = 0;

/// Reproject one WKT geometry string. Returns None on any failure.
fn transform_wkt(wkt_in: &str, from_srid: i32, to_srid: i32) -> Option<String> {
    use std::str::FromStr;
    if from_srid <= 0 || to_srid <= 0 {
        return None;
    }

    // Build source + target spatial references from EPSG codes (embedded proj.db).
    let src = srs::SpatialRef::from_epsg(from_srid as u32).ok()?;
    src.set_axis_mapping_strategy(TRADITIONAL_GIS_ORDER);
    let dst = srs::SpatialRef::from_epsg(to_srid as u32).ok()?;
    dst.set_axis_mapping_strategy(TRADITIONAL_GIS_ORDER);

    // Coordinate transformation between the two CRS.
    let xform = srs::Transform::new(&src, &dst);

    // Parse WKT -> geo-types geometry, walk every coordinate, reproject in place.
    let geom = wkt::Wkt::<f64>::from_str(wkt_in).ok()?;
    let mut g: geo_types::Geometry<f64> = geom.try_into().ok()?;
    reproject_geometry(&mut g, &xform)?;

    // Re-emit as WKT.
    use wkt::ToWkt;
    Some(g.wkt_string())
}

/// Reproject every coordinate of a geo-types geometry through the transform.
fn reproject_geometry(g: &mut geo_types::Geometry<f64>, xform: &srs::Transform) -> Option<()> {
    use geo_types::Geometry::*;
    match g {
        Point(p) => reproject_coord(&mut p.0, xform),
        Line(l) => {
            reproject_coord(&mut l.start, xform)?;
            reproject_coord(&mut l.end, xform)
        }
        LineString(ls) => reproject_coords(ls.0.iter_mut(), xform),
        Polygon(poly) => reproject_polygon(poly, xform),
        MultiPoint(mp) => {
            for p in mp.0.iter_mut() {
                reproject_coord(&mut p.0, xform)?;
            }
            Some(())
        }
        MultiLineString(mls) => {
            for ls in mls.0.iter_mut() {
                reproject_coords(ls.0.iter_mut(), xform)?;
            }
            Some(())
        }
        MultiPolygon(mpoly) => {
            for poly in mpoly.0.iter_mut() {
                reproject_polygon(poly, xform)?;
            }
            Some(())
        }
        GeometryCollection(gc) => {
            for inner in gc.0.iter_mut() {
                reproject_geometry(inner, xform)?;
            }
            Some(())
        }
        Rect(_) | Triangle(_) => None,
    }
}

fn reproject_polygon(poly: &mut geo_types::Polygon<f64>, xform: &srs::Transform) -> Option<()> {
    // geo-types Polygon fields are private; rebuild from reprojected rings.
    let mut ext: geo_types::LineString<f64> = poly.exterior().clone();
    reproject_coords(ext.0.iter_mut(), xform)?;
    let mut ints: Vec<geo_types::LineString<f64>> = poly.interiors().to_vec();
    for ring in ints.iter_mut() {
        reproject_coords(ring.0.iter_mut(), xform)?;
    }
    *poly = geo_types::Polygon::new(ext, ints);
    Some(())
}

fn reproject_coords<'a, I>(coords: I, xform: &srs::Transform) -> Option<()>
where
    I: Iterator<Item = &'a mut geo_types::Coord<f64>>,
{
    for c in coords {
        reproject_coord(c, xform)?;
    }
    Some(())
}

fn reproject_coord(c: &mut geo_types::Coord<f64>, xform: &srs::Transform) -> Option<()> {
    let (x, y, _z) = xform.transform_point(c.x, c.y, 0.0).ok()?;
    c.x = x;
    c.y = y;
    Some(())
}

fn manifest() -> Result<Vec<u8>, Error> {
    #[derive(serde::Serialize)]
    struct Manifest {
        name: &'static str,
        version: &'static str,
        methods: Vec<&'static str>,
    }
    encode(&Manifest {
        name: "gdal-endpoint",
        version: env!("CARGO_PKG_VERSION"),
        methods: vec!["manifest", "transform"],
    })
}

fn op_transform(payload: &[u8]) -> Result<Vec<u8>, Error> {
    let req: TransformReq = decode(payload)?;
    match transform_wkt(&req.wkt, req.from_srid, req.to_srid) {
        Some(wkt) => encode(&TransformResp { wkt }),
        None => Err(err(
            ErrorCode::InvalidInput,
            format!(
                "gdal-endpoint: transform failed (from_srid={}, to_srid={})",
                req.from_srid, req.to_srid
            ),
            // The consumer shim maps this Err -> SQL NULL, matching the
            // original spatialproj ST_Transform semantics.
            "transform-failed",
        )),
    }
}

impl Guest for GdalEndpoint {
    fn handle(method: String, payload: Vec<u8>) -> Result<Vec<u8>, Error> {
        match method.as_str() {
            "manifest" => manifest(),
            "transform" => op_transform(&payload),
            other => Err(err(
                ErrorCode::NotImplemented,
                format!("gdal-endpoint: unknown method '{other}'"),
                "invalid-request",
            )),
        }
    }
}

export!(GdalEndpoint);
