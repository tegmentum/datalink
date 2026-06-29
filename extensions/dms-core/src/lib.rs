//! Neutral core for the `dms` extension — geographic coordinate conversion
//! between degrees/minutes/seconds and decimal degrees — written ONCE. The
//! per-DB shims are generated from the [`declare!`](datalink_extcore::declare)
//! table below.
//!
//! # Functions
//!
//!   * `dms_to_decimal(deg, min, sec) -> float64`  (sign follows `deg`)
//!   * `decimal_to_dms(decimal)       -> text`     (`D°M'S.s"`)
//!
//! `std` (not `no_std`): the coordinate math uses `f64` trunc/abs intrinsics
//! that live in `std`. `extern crate alloc` keeps the `declare!`-generated
//! `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    /// DMS -> decimal degrees; the sign follows the degree component.
    pub fn to_decimal(d: f64, m: f64, s: f64) -> f64 {
        let sign = if d.is_sign_negative() { -1.0 } else { 1.0 };
        sign * (d.abs() + m / 60.0 + s / 3600.0)
    }

    /// Decimal degrees -> `D°M'S.s"`.
    pub fn to_dms(dec: f64) -> String {
        let neg = dec.is_sign_negative();
        let a = dec.abs();
        let deg = a.trunc();
        let rem = (a - deg) * 60.0;
        let min = rem.trunc();
        let sec = (rem - min) * 60.0;
        alloc::format!(
            "{}{}\u{00b0}{}'{:.1}\"",
            if neg { "-" } else { "" },
            deg as i64,
            min as i64,
            sec
        )
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "dms";
    version = env!("CARGO_PKG_VERSION");

    scalar dms_to_decimal(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        let d = args.arg_float(0, "dms_to_decimal")?;
        let m = args.arg_float(1, "dms_to_decimal")?;
        let s = args.arg_float(2, "dms_to_decimal")?;
        Ok(NeutralValue::Float64(logic::to_decimal(d, m, s)))
    };

    scalar decimal_to_dms(float64) -> text [propagate, deterministic] = |args| {
        let dec = args.arg_float(0, "decimal_to_dms")?;
        Ok(NeutralValue::Text(logic::to_dms(dec)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;

    fn f(v: f64) -> NeutralValue {
        NeutralValue::Float64(v)
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn as_f64(v: NeutralValue) -> f64 {
        match v {
            NeutralValue::Float64(x) => x,
            other => panic!("expected float, got {:?}", other),
        }
    }

    #[test]
    fn parity_with_baseline_smoke() {
        let dec = as_f64(Core::dispatch(idx("dms_to_decimal"), &[f(40.0), f(26.0), f(46.0)]).unwrap());
        assert!((dec - 40.446111).abs() < 1e-6, "got {dec}");
        let neg = as_f64(Core::dispatch(idx("dms_to_decimal"), &[f(-73.0), f(58.0), f(23.0)]).unwrap());
        assert!((neg - (-73.973056)).abs() < 1e-6, "got {neg}");
        assert_eq!(
            Core::dispatch(idx("decimal_to_dms"), &[f(40.446111)]).unwrap(),
            NeutralValue::Text(String::from("40\u{00b0}26'46.0\""))
        );
        assert_eq!(
            Core::dispatch(idx("decimal_to_dms"), &[f(-73.97306)]).unwrap(),
            NeutralValue::Text(String::from("-73\u{00b0}58'23.0\""))
        );
    }

    #[test]
    fn integer_args_widen() {
        // dms_to_decimal accepts INT64 args (widened), matching the baseline.
        let dec = as_f64(Core::dispatch(idx("dms_to_decimal"), &[NeutralValue::Int64(40), NeutralValue::Int64(26), NeutralValue::Int64(46)]).unwrap());
        assert!((dec - 40.446111).abs() < 1e-6, "got {dec}");
    }
}
