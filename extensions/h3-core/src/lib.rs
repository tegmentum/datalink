//! Neutral core for the `h3` extension — Uber H3 hexagonal geospatial
//! indexing via `h3o` — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `h3_latlng_to_cell(lat float64, lng float64, res int64) -> int64`
//!   * `h3_cell_to_lat(cell int64) -> float64`
//!   * `h3_cell_to_lng(cell int64) -> float64`
//!   * `h3_cell_to_parent(cell int64, res int64) -> int64`
//!   * `h3_grid_distance(a int64, b int64) -> int64`
//!   * `h3_is_valid_cell(cell int64) -> boolean`
//!
//! NULL / invalid input -> NULL (never panics); `h3_is_valid_cell`
//! returns BOOLEAN (`false` for a non-cell BIGINT). The surface is
//! identical in both ports (zero drift).

extern crate alloc;

use core::convert::TryFrom;
use datalink_extcore::NeutralValue;
use h3o::{CellIndex, LatLng, Resolution};

/// Parse a BIGINT into a valid H3 cell index. The i64 is bit-reinterpreted
/// as u64 (valid H3 indices have bit 63 clear, so they are always
/// non-negative i64 values).
fn cell(raw: i64) -> Option<CellIndex> {
    CellIndex::try_from(raw as u64).ok()
}

fn resolution(raw: i64) -> Option<Resolution> {
    let r = u8::try_from(raw).ok()?;
    Resolution::try_from(r).ok()
}

fn cell_to_i64(c: CellIndex) -> i64 {
    u64::from(c) as i64
}

datalink_extcore::declare! {
    core = Core;
    extension = "h3";
    version = env!("CARGO_PKG_VERSION");

    scalar h3_latlng_to_cell(float64, float64, int64) -> int64 [propagate, deterministic] = |args| {
        let lat = args.arg_float(0, "h3_latlng_to_cell")?;
        let lng = args.arg_float(1, "h3_latlng_to_cell")?;
        let res = match resolution(args.arg_int(2, "h3_latlng_to_cell")?) {
            Some(r) => r,
            None => return Ok(NeutralValue::Null),
        };
        Ok(match LatLng::new(lat, lng) {
            Ok(ll) => NeutralValue::Int64(cell_to_i64(ll.to_cell(res))),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar h3_cell_to_lat(int64) -> float64 [propagate, deterministic] = |args| {
        Ok(match cell(args.arg_int(0, "h3_cell_to_lat")?) {
            Some(c) => { let ll: LatLng = c.into(); NeutralValue::Float64(ll.lat()) }
            None => NeutralValue::Null,
        })
    };
    scalar h3_cell_to_lng(int64) -> float64 [propagate, deterministic] = |args| {
        Ok(match cell(args.arg_int(0, "h3_cell_to_lng")?) {
            Some(c) => { let ll: LatLng = c.into(); NeutralValue::Float64(ll.lng()) }
            None => NeutralValue::Null,
        })
    };
    scalar h3_cell_to_parent(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let c = match cell(args.arg_int(0, "h3_cell_to_parent")?) {
            Some(c) => c,
            None => return Ok(NeutralValue::Null),
        };
        let res = match resolution(args.arg_int(1, "h3_cell_to_parent")?) {
            Some(r) => r,
            None => return Ok(NeutralValue::Null),
        };
        Ok(match c.parent(res) {
            Some(p) => NeutralValue::Int64(cell_to_i64(p)),
            None => NeutralValue::Null,
        })
    };
    scalar h3_grid_distance(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let a = match cell(args.arg_int(0, "h3_grid_distance")?) {
            Some(c) => c,
            None => return Ok(NeutralValue::Null),
        };
        let b = match cell(args.arg_int(1, "h3_grid_distance")?) {
            Some(c) => c,
            None => return Ok(NeutralValue::Null),
        };
        Ok(match a.grid_distance(b) {
            Ok(d) => NeutralValue::Int64(d.into()),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar h3_is_valid_cell(int64) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_int(0, "h3_is_valid_cell")?;
        Ok(NeutralValue::Boolean(CellIndex::try_from(raw as u64).is_ok()))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        let cell = match Core::dispatch(idx("h3_latlng_to_cell"), &[NeutralValue::Float64(37.775), NeutralValue::Float64(-122.418), NeutralValue::Int64(9)]).unwrap() {
            NeutralValue::Int64(c) => c, o => panic!("{o:?}"),
        };
        assert_eq!(cell, 617700169957507071);
        assert_eq!(Core::dispatch(idx("h3_is_valid_cell"), &[NeutralValue::Int64(cell)]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("h3_is_valid_cell"), &[NeutralValue::Int64(123)]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("h3_grid_distance"), &[NeutralValue::Int64(cell), NeutralValue::Int64(cell)]).unwrap(), NeutralValue::Int64(0));
    }
}
