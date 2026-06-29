//! Neutral core for the `a5` extension — the A5 pentagonal discrete global
//! grid system (DGGS) via the `a5` crate — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table. Mirrors
//! the `h3` surface (cell ids are int64).
//!
//!   * `a5_lonlat_to_cell(lat float64, lon float64, resolution int64) -> int64`
//!   * `a5_cell_to_lat(cell int64) -> float64`
//!   * `a5_cell_to_lon(cell int64) -> float64`
//!   * `a5_cell_to_resolution(cell int64) -> int64`
//!   * `a5_cell_to_parent(cell int64, resolution int64) -> int64`
//!   * `a5_is_valid_cell(cell int64) -> boolean`
//!   * `a5_cell_to_hex(cell int64) -> text`
//!   * `a5_hex_to_cell(hex text) -> int64`
//!
//! A5 cell indices are `u64`; here they are reinterpreted bit-for-bit as
//! `int64` so they round-trip through a DuckDB BIGINT column (the same trick
//! `h3` uses). Out-of-range coordinates, an invalid cell, or a bad hex string
//! yield NULL (never a panic). NULL inputs propagate to NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic A5 logic. Cells are exposed to SQL as `i64` (the `u64` A5
/// index reinterpreted bit-for-bit); these helpers bridge that boundary.
pub mod logic {
    use a5::{Degrees, LonLat};

    #[inline]
    fn to_i64(cell: u64) -> i64 {
        cell as i64
    }
    #[inline]
    fn to_u64(cell: i64) -> u64 {
        cell as u64
    }

    /// Encode (lat, lon) at `resolution` to an A5 cell. `None` on error.
    pub fn lonlat_to_cell(lat: f64, lon: f64, resolution: i64) -> Option<i64> {
        if !(lat.is_finite() && lon.is_finite()) {
            return None;
        }
        let res = i32::try_from(resolution).ok()?;
        a5::lonlat_to_cell(LonLat::new(lon, lat), res).ok().map(to_i64)
    }

    /// Latitude of a cell's center. `None` on an invalid cell.
    pub fn cell_to_lat(cell: i64) -> Option<f64> {
        a5::cell_to_lonlat(to_u64(cell))
            .ok()
            .map(|ll: LonLat| ll.latitude.0)
    }

    /// Longitude of a cell's center. `None` on an invalid cell.
    pub fn cell_to_lon(cell: i64) -> Option<f64> {
        a5::cell_to_lonlat(to_u64(cell))
            .ok()
            .map(|ll: LonLat| ll.longitude.0)
    }

    /// Resolution of a cell, or `None` if it is the (resolution-less) world
    /// cell / invalid.
    pub fn cell_to_resolution(cell: i64) -> Option<i64> {
        let r = a5::get_resolution(to_u64(cell));
        if r < 0 {
            None
        } else {
            Some(r as i64)
        }
    }

    /// Parent cell at `resolution`. `None` on error.
    pub fn cell_to_parent(cell: i64, resolution: i64) -> Option<i64> {
        let res = i32::try_from(resolution).ok()?;
        a5::cell_to_parent(to_u64(cell), Some(res)).ok().map(to_i64)
    }

    /// True iff `cell` decodes to a valid A5 cell.
    pub fn is_valid_cell(cell: i64) -> bool {
        a5::cell_to_lonlat(to_u64(cell)).is_ok()
    }

    /// Hex (base-16) rendering of the cell index.
    pub fn cell_to_hex(cell: i64) -> alloc::string::String {
        a5::u64_to_hex(to_u64(cell))
    }

    /// Parse a hex cell index. `None` on a malformed string.
    pub fn hex_to_cell(hex: &str) -> Option<i64> {
        a5::hex_to_u64(hex.trim()).ok().map(to_i64)
    }

    // Touch `Degrees` so the import is always meaningful regardless of
    // upstream field-access changes.
    #[allow(dead_code)]
    fn _assert_degrees(d: Degrees) -> f64 {
        d.0
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "a5";
    version = env!("CARGO_PKG_VERSION");

    scalar a5_lonlat_to_cell(float64, float64, int64) -> int64 [propagate, deterministic] = |args| {
        let lat = args.arg_float(0, "a5_lonlat_to_cell")?;
        let lon = args.arg_float(1, "a5_lonlat_to_cell")?;
        let res = args.arg_int(2, "a5_lonlat_to_cell")?;
        Ok(match logic::lonlat_to_cell(lat, lon, res) {
            Some(c) => NeutralValue::Int64(c),
            None => NeutralValue::Null,
        })
    };

    scalar a5_cell_to_lat(int64) -> float64 [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_cell_to_lat")?;
        Ok(match logic::cell_to_lat(cell) {
            Some(v) => NeutralValue::Float64(v),
            None => NeutralValue::Null,
        })
    };

    scalar a5_cell_to_lon(int64) -> float64 [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_cell_to_lon")?;
        Ok(match logic::cell_to_lon(cell) {
            Some(v) => NeutralValue::Float64(v),
            None => NeutralValue::Null,
        })
    };

    scalar a5_cell_to_resolution(int64) -> int64 [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_cell_to_resolution")?;
        Ok(match logic::cell_to_resolution(cell) {
            Some(v) => NeutralValue::Int64(v),
            None => NeutralValue::Null,
        })
    };

    scalar a5_cell_to_parent(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_cell_to_parent")?;
        let res = args.arg_int(1, "a5_cell_to_parent")?;
        Ok(match logic::cell_to_parent(cell, res) {
            Some(v) => NeutralValue::Int64(v),
            None => NeutralValue::Null,
        })
    };

    scalar a5_is_valid_cell(int64) -> boolean [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_is_valid_cell")?;
        Ok(NeutralValue::Boolean(logic::is_valid_cell(cell)))
    };

    scalar a5_cell_to_hex(int64) -> text [propagate, deterministic] = |args| {
        let cell = args.arg_int(0, "a5_cell_to_hex")?;
        Ok(NeutralValue::Text(logic::cell_to_hex(cell)))
    };

    scalar a5_hex_to_cell(text) -> int64 [propagate, deterministic] = |args| {
        let hex = args.arg_text(0, "a5_hex_to_cell")?;
        Ok(match logic::hex_to_cell(&hex) {
            Some(v) => NeutralValue::Int64(v),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(alloc::string::String::from(s))
    }

    #[test]
    fn roundtrips_a_known_point() {
        // Encode the Empire State Building, then decode the center back: the
        // recovered point must be near the input (cells are small at res 10).
        let cell = match Core::dispatch(
            idx("a5_lonlat_to_cell"),
            &[NeutralValue::Float64(40.7484), NeutralValue::Float64(-73.9857), NeutralValue::Int64(10)],
        )
        .unwrap()
        {
            NeutralValue::Int64(c) => c,
            other => panic!("expected cell int64, got {other:?}"),
        };

        match Core::dispatch(idx("a5_cell_to_lat"), &[NeutralValue::Int64(cell)]).unwrap() {
            NeutralValue::Float64(v) => assert!((v - 40.7484).abs() < 1.0, "lat {v}"),
            other => panic!("expected lat float, got {other:?}"),
        }
        match Core::dispatch(idx("a5_cell_to_lon"), &[NeutralValue::Int64(cell)]).unwrap() {
            NeutralValue::Float64(v) => assert!((v + 73.9857).abs() < 1.0, "lon {v}"),
            other => panic!("expected lon float, got {other:?}"),
        }
        assert_eq!(
            Core::dispatch(idx("a5_is_valid_cell"), &[NeutralValue::Int64(cell)]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("a5_cell_to_resolution"), &[NeutralValue::Int64(cell)]).unwrap(),
            NeutralValue::Int64(10)
        );
    }

    #[test]
    fn hex_roundtrips() {
        let cell = match Core::dispatch(
            idx("a5_lonlat_to_cell"),
            &[NeutralValue::Float64(0.0), NeutralValue::Float64(0.0), NeutralValue::Int64(5)],
        )
        .unwrap()
        {
            NeutralValue::Int64(c) => c,
            other => panic!("expected cell, got {other:?}"),
        };
        let hex = Core::dispatch(idx("a5_cell_to_hex"), &[NeutralValue::Int64(cell)]).unwrap();
        let back = Core::dispatch(idx("a5_hex_to_cell"), &[hex]).unwrap();
        assert_eq!(back, NeutralValue::Int64(cell));
    }

    #[test]
    fn bad_hex_is_null() {
        assert_eq!(
            Core::dispatch(idx("a5_hex_to_cell"), &[t("not-hex")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn parent_has_lower_resolution() {
        let cell = match Core::dispatch(
            idx("a5_lonlat_to_cell"),
            &[NeutralValue::Float64(51.5074), NeutralValue::Float64(-0.1278), NeutralValue::Int64(8)],
        )
        .unwrap()
        {
            NeutralValue::Int64(c) => c,
            other => panic!("expected cell, got {other:?}"),
        };
        match Core::dispatch(idx("a5_cell_to_parent"), &[NeutralValue::Int64(cell), NeutralValue::Int64(5)]).unwrap() {
            NeutralValue::Int64(parent) => {
                assert_eq!(
                    Core::dispatch(idx("a5_cell_to_resolution"), &[NeutralValue::Int64(parent)]).unwrap(),
                    NeutralValue::Int64(5)
                );
            }
            other => panic!("expected parent cell, got {other:?}"),
        }
    }
}
