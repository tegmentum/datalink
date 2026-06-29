//! Neutral core for the `haversine` extension — great-circle distance —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `haversine_km(lat1, lon1, lat2, lon2 double) -> double`.
//!   * `haversine_mi(lat1, lon1, lat2, lon2 double) -> double`.
//!
//! NULL argument -> NULL (propagate). INTEGER coordinates widen to double
//! (matching the pre-pullup `f` helper). Identical in both ports.
//!
//! NOTE: this core depends on `std` (not `#![no_std]`) because it uses the
//! inherent `f64` transcendental methods (`sin`/`cos`/`sqrt`/`asin`), which
//! live in `std`, not `core`.

extern crate alloc;

use datalink_extcore::{ArgExt, NeutralValue};

const EARTH_KM: f64 = 6371.0088;
const KM_PER_MI: f64 = 0.621371192;

pub fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dlat, dlon) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    EARTH_KM * 2.0 * a.sqrt().asin()
}

datalink_extcore::declare! {
    core = Core;
    extension = "haversine";
    version = env!("CARGO_PKG_VERSION");

    scalar haversine_km(float64, float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_float(0, "haversine_km")?;
        let b = args.arg_float(1, "haversine_km")?;
        let c = args.arg_float(2, "haversine_km")?;
        let d = args.arg_float(3, "haversine_km")?;
        Ok(NeutralValue::Float64(distance_km(a, b, c, d)))
    };

    scalar haversine_mi(float64, float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_float(0, "haversine_mi")?;
        let b = args.arg_float(1, "haversine_mi")?;
        let c = args.arg_float(2, "haversine_mi")?;
        let d = args.arg_float(3, "haversine_mi")?;
        Ok(NeutralValue::Float64(distance_km(a, b, c, d) * KM_PER_MI))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn fv(x: f64) -> NeutralValue {
        NeutralValue::Float64(x)
    }

    #[test]
    fn matches_baseline() {
        // London -> Paris, roughly 343 km.
        let args = [fv(51.5074), fv(-0.1278), fv(48.8566), fv(2.3522)];
        let km = match Core::dispatch(idx("haversine_km"), &args).unwrap() {
            NeutralValue::Float64(x) => x,
            other => panic!("{other:?}"),
        };
        assert!((km - 343.5).abs() < 2.0, "km={km}");
        // Same value, scaled to miles via the inline helper.
        assert_eq!(
            Core::dispatch(idx("haversine_mi"), &args).unwrap(),
            fv(distance_km(51.5074, -0.1278, 48.8566, 2.3522) * KM_PER_MI)
        );
    }
}
