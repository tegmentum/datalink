//! Neutral core for the `celestial` extension — spherical astronomy (J2000)
//! coordinate conversions — written ONCE. The per-DB shims are generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `equatorial_to_galactic_l(ra_deg, dec_deg) -> float64`
//!   * `equatorial_to_galactic_b(ra_deg, dec_deg) -> float64`
//!   * `angular_separation(ra1, dec1, ra2, dec2) -> float64`
//!   * `hms_to_deg(h, m, s) -> float64`
//!   * `dms_to_deg(d, m, s) -> float64`
//!
//! A FLOAT64 or INT64 argument is accepted (the shim widens INT64 -> f64).
//! Any NULL argument propagates to a NULL result.
//!
//! Not `#![no_std]`: the logic uses f64 transcendentals (sin/cos/atan2/asin/
//! sqrt), which live in `std` (the libm intrinsics), not `core`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    // Galactic-pole constants in the J2000 frame (IAU 1958, precessed to J2000):
    //   North Galactic Pole:  RA = 192.85948 deg, Dec = +27.12825 deg
    //   Galactic longitude of the North Celestial Pole: l_NCP = 122.93192 deg
    const RA_NGP_DEG: f64 = 192.85948;
    const DEC_NGP_DEG: f64 = 27.12825;
    const L_NCP_DEG: f64 = 122.93192;

    const DEG2RAD: f64 = std::f64::consts::PI / 180.0;
    const RAD2DEG: f64 = 180.0 / std::f64::consts::PI;

    pub fn galactic_l(ra_deg: f64, dec_deg: f64) -> f64 {
        let ra = ra_deg * DEG2RAD;
        let dec = dec_deg * DEG2RAD;
        let ra_ngp = RA_NGP_DEG * DEG2RAD;
        let dec_ngp = DEC_NGP_DEG * DEG2RAD;
        let y = dec.cos() * (ra - ra_ngp).sin();
        let x = dec.sin() * dec_ngp.cos() - dec.cos() * dec_ngp.sin() * (ra - ra_ngp).cos();
        let mut l = L_NCP_DEG - y.atan2(x) * RAD2DEG;
        l = l.rem_euclid(360.0);
        l
    }

    pub fn galactic_b(ra_deg: f64, dec_deg: f64) -> f64 {
        let ra = ra_deg * DEG2RAD;
        let dec = dec_deg * DEG2RAD;
        let ra_ngp = RA_NGP_DEG * DEG2RAD;
        let dec_ngp = DEC_NGP_DEG * DEG2RAD;
        let sin_b = dec.sin() * dec_ngp.sin() + dec.cos() * dec_ngp.cos() * (ra - ra_ngp).cos();
        sin_b.clamp(-1.0, 1.0).asin() * RAD2DEG
    }

    /// Great-circle angle between two equatorial points (degrees), haversine form.
    pub fn angular_separation(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
        let d1 = dec1 * DEG2RAD;
        let d2 = dec2 * DEG2RAD;
        let dra = (ra2 - ra1) * DEG2RAD;
        let ddec = d2 - d1;
        let h = (ddec / 2.0).sin().powi(2) + d1.cos() * d2.cos() * (dra / 2.0).sin().powi(2);
        2.0 * h.sqrt().clamp(0.0, 1.0).asin() * RAD2DEG
    }

    /// Hours-minutes-seconds of RA -> degrees (15 deg per hour). Sign from `h`.
    pub fn hms_to_deg(h: f64, m: f64, s: f64) -> f64 {
        let sign = if h.is_sign_negative() { -1.0 } else { 1.0 };
        sign * (h.abs() + m / 60.0 + s / 3600.0) * 15.0
    }

    /// Degrees-minutes-seconds -> decimal degrees. Sign from `d`.
    pub fn dms_to_deg(d: f64, m: f64, s: f64) -> f64 {
        let sign = if d.is_sign_negative() { -1.0 } else { 1.0 };
        sign * (d.abs() + m / 60.0 + s / 3600.0)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "celestial";
    version = env!("CARGO_PKG_VERSION");

    scalar equatorial_to_galactic_l(float64, float64) -> float64 [propagate, deterministic] = |args| {
        let ra = args.arg_float(0, "equatorial_to_galactic_l")?;
        let dec = args.arg_float(1, "equatorial_to_galactic_l")?;
        Ok(NeutralValue::Float64(logic::galactic_l(ra, dec)))
    };

    scalar equatorial_to_galactic_b(float64, float64) -> float64 [propagate, deterministic] = |args| {
        let ra = args.arg_float(0, "equatorial_to_galactic_b")?;
        let dec = args.arg_float(1, "equatorial_to_galactic_b")?;
        Ok(NeutralValue::Float64(logic::galactic_b(ra, dec)))
    };

    scalar angular_separation(float64, float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let ra1 = args.arg_float(0, "angular_separation")?;
        let dec1 = args.arg_float(1, "angular_separation")?;
        let ra2 = args.arg_float(2, "angular_separation")?;
        let dec2 = args.arg_float(3, "angular_separation")?;
        Ok(NeutralValue::Float64(logic::angular_separation(ra1, dec1, ra2, dec2)))
    };

    scalar hms_to_deg(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let h = args.arg_float(0, "hms_to_deg")?;
        let m = args.arg_float(1, "hms_to_deg")?;
        let s = args.arg_float(2, "hms_to_deg")?;
        Ok(NeutralValue::Float64(logic::hms_to_deg(h, m, s)))
    };

    scalar dms_to_deg(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let d = args.arg_float(0, "dms_to_deg")?;
        let m = args.arg_float(1, "dms_to_deg")?;
        let s = args.arg_float(2, "dms_to_deg")?;
        Ok(NeutralValue::Float64(logic::dms_to_deg(d, m, s)))
    };
}
