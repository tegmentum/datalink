//! Neutral core for the `geohash` extension — geohash encode/decode via
//! the `geohash` crate — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `geohash_encode(lat float64, lon float64, precision int64) -> text`
//!   * `geohash_decode_lat(hash) -> float64`
//!   * `geohash_decode_lon(hash) -> float64`
//!
//! NULL / invalid -> NULL. The surface is identical in both ports (zero
//! drift). Exercises [`NeutralValue::Float64`] + the `arg_float` helper.

extern crate alloc;

use datalink_extcore::NeutralValue;
use geohash::Coord;

datalink_extcore::declare! {
    core = Core;
    extension = "geohash";
    version = env!("CARGO_PKG_VERSION");

    scalar geohash_encode(float64, float64, int64) -> text [propagate, deterministic] = |args| {
        let lat = args.arg_float(0, "geohash_encode")?;
        let lon = args.arg_float(1, "geohash_encode")?;
        // Match the pre-pullup default: a non-positive / absent precision
        // falls back to 9.
        let len = match args.get(2) {
            Some(NeutralValue::Int64(n)) if *n > 0 => *n as usize,
            _ => 9,
        };
        Ok(match geohash::encode(Coord { x: lon, y: lat }, len) {
            Ok(s) => NeutralValue::Text(s),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar geohash_decode_lat(text) -> float64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "geohash_decode_lat")?;
        Ok(match geohash::decode(&s) {
            Ok((c, _, _)) => NeutralValue::Float64(c.y),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar geohash_decode_lon(text) -> float64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "geohash_decode_lon")?;
        Ok(match geohash::decode(&s) {
            Ok((c, _, _)) => NeutralValue::Float64(c.x),
            Err(_) => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(alloc::string::String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(
            Core::dispatch(idx("geohash_encode"), &[NeutralValue::Float64(40.7484), NeutralValue::Float64(-73.9857), NeutralValue::Int64(5)]).unwrap(),
            t("dr5ru")
        );
        match Core::dispatch(idx("geohash_decode_lat"), &[t("dr5ru")]).unwrap() {
            NeutralValue::Float64(v) => assert!((v - 40.76).abs() < 0.1),
            other => panic!("expected float, got {other:?}"),
        }
        assert_eq!(Core::dispatch(idx("geohash_decode_lat"), &[t("not!valid")]).unwrap(), NeutralValue::Null);
    }
}
