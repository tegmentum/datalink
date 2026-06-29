//! Neutral core for the `vssfns` extension — the genuinely-missing vector math
//! from DuckDB's `vss` (L2 / cosine / inner-product already live in
//! core_functions) — written ONCE. The per-DB shims (ducklink
//! `duckdb:extension`, sqlink `sqlite:extension`, sqlink embed) are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//! Vectors cross as JSON number arrays in VARCHAR (the WIT value surface is
//! scalar-only). Everything is NULL-safe and never panics: NULL in, bad JSON,
//! length mismatch, or non-finite -> NULL.
//!
//! # Functions
//!
//!   * `vec_l1_distance(a, b) -> double`   Manhattan / L1 distance
//!   * `vec_linf_distance(a, b) -> double` Chebyshev / L-infinity distance
//!   * `vec_normalize(a) -> text`          unit vector (JSON array), L2-normalized

extern crate alloc;

use datalink_extcore::{ArgExt, NeutralValue};

/// Pure vector math, byte-for-byte the pre-pullup logic (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Parse a JSON array of finite numbers into `Vec<f64>`. Any non-array, any
    /// non-number element, or any non-finite (NaN/Inf) element -> None.
    pub fn parse_vec(s: &str) -> Option<Vec<f64>> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        let arr = v.as_array()?;
        let mut out = Vec::with_capacity(arr.len());
        for e in arr {
            let x = e.as_f64()?;
            if !x.is_finite() {
                return None;
            }
            out.push(x);
        }
        Some(out)
    }

    /// L1 (Manhattan) distance. Length mismatch, empty, parse failure, or
    /// non-finite -> None.
    pub fn l1(a: &str, b: &str) -> Option<f64> {
        let a = parse_vec(a)?;
        let b = parse_vec(b)?;
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        let d: f64 = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum();
        if d.is_finite() {
            Some(d)
        } else {
            None
        }
    }

    /// L-infinity (Chebyshev) distance. Same failure modes as [`l1`].
    pub fn linf(a: &str, b: &str) -> Option<f64> {
        let a = parse_vec(a)?;
        let b = parse_vec(b)?;
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        let d: f64 = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f64, f64::max);
        if d.is_finite() {
            Some(d)
        } else {
            None
        }
    }

    /// L2-normalize a vector to a unit vector, rendered as a JSON array string.
    /// Parse failure, empty, or zero/near-zero magnitude -> None.
    pub fn normalize(a: &str) -> Option<String> {
        let v = parse_vec(a)?;
        if v.is_empty() {
            return None;
        }
        let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return None;
        }
        let unit: Vec<f64> = v.iter().map(|x| x / norm).collect();
        if unit.iter().any(|x| !x.is_finite()) {
            return None;
        }
        serde_json::to_string(&unit).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "vssfns";
    version = env!("CARGO_PKG_VERSION");

    scalar vec_l1_distance(text, text) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "vec_l1_distance")?;
        let b = args.arg_text(1, "vec_l1_distance")?;
        Ok(match logic::l1(&a, &b) {
            Some(d) => NeutralValue::Float64(d),
            None => NeutralValue::Null,
        })
    };

    scalar vec_linf_distance(text, text) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "vec_linf_distance")?;
        let b = args.arg_text(1, "vec_linf_distance")?;
        Ok(match logic::linf(&a, &b) {
            Some(d) => NeutralValue::Float64(d),
            None => NeutralValue::Null,
        })
    };

    scalar vec_normalize(text) -> text [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "vec_normalize")?;
        Ok(match logic::normalize(&a) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn distances() {
        assert_eq!(
            Core::dispatch(idx("vec_l1_distance"), &[t("[1, 2, 3]"), t("[4, 6, 3]")]).unwrap(),
            NeutralValue::Float64(7.0)
        );
        assert_eq!(
            Core::dispatch(idx("vec_linf_distance"), &[t("[1, 2, 3]"), t("[4, 6, 3]")]).unwrap(),
            NeutralValue::Float64(4.0)
        );
        assert_eq!(
            Core::dispatch(idx("vec_l1_distance"), &[t("[1, 2, 3]"), t("[1, 2]")]).unwrap(),
            NeutralValue::Null
        );
        assert_eq!(
            Core::dispatch(idx("vec_l1_distance"), &[t("garbage"), t("[1, 2]")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn normalize() {
        assert_eq!(
            Core::dispatch(idx("vec_normalize"), &[t("[3, 4]")]).unwrap(),
            NeutralValue::Text(String::from("[0.6,0.8]"))
        );
        assert_eq!(
            Core::dispatch(idx("vec_normalize"), &[t("[0, 0, 0]")]).unwrap(),
            NeutralValue::Null
        );
    }
}
