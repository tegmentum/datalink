//! Neutral core for the `aggstat` extension — statistical AGGREGATES
//! DuckDB core lacks — written ONCE. The per-DB aggregate shim is
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `harmonic_mean(x) -> double`  the harmonic mean of the non-NULL,
//!     non-zero values in a group: `n / sum(1/x_i)`. An empty group (or
//!     one with no usable values) yields NULL.
//!
//! # The aggregate fold
//!
//! The neutral `state` is `(n, recip_sum)`; `step` folds each row
//! (skipping NULL and zero, matching the hand-written aggregate);
//! `finalize` emits the mean (or NULL). On the `duckdb:extension` host
//! the whole fold runs in one `call_aggregate`, so the state is a native
//! Rust value and never crosses the WIT boundary.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Extract a numeric value as `f64`, accepting the neutral arms DuckDB
/// can produce for a `DOUBLE`-typed aggregate argument (the host coerces
/// the column to the registered `Float64`; an integer literal widens).
/// NULL (and anything else) yields `None` so `step` skips it — matching
/// the hand-written `as_f64` row skip.
fn num(v: &NeutralValue) -> Option<f64> {
    match v {
        NeutralValue::Float64(x) => Some(*x),
        NeutralValue::Int64(x) => Some(*x as f64),
        _ => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "aggstat";
    version = env!("CARGO_PKG_VERSION");

    aggregate harmonic_mean(float64) -> float64 [deterministic] {
        state = (u64, f64);
        init = (0u64, 0.0f64);
        step = |st: &mut (u64, f64), row: &[NeutralValue]| {
            if let Some(x) = row.first().and_then(num) {
                if x != 0.0 {
                    st.0 += 1;
                    st.1 += 1.0 / x;
                }
            }
        };
        finalize = |st: (u64, f64)| {
            if st.0 == 0 || st.1 == 0.0 {
                Ok(NeutralValue::Null)
            } else {
                Ok(NeutralValue::Float64(st.0 as f64 / st.1))
            }
        };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;

    fn f(x: f64) -> NeutralValue {
        NeutralValue::Float64(x)
    }

    #[test]
    fn harmonic_mean_multi_row() {
        // 3 / (1 + 0.5 + 0.25) = 1.714285...
        let rows = [&[f(1.0)][..], &[f(2.0)][..], &[f(4.0)][..]];
        let got = Core::dispatch_aggregate(0, &rows).unwrap();
        match got {
            NeutralValue::Float64(v) => assert!((v - 1.714285).abs() < 1e-5),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn empty_group_is_null() {
        assert_eq!(
            Core::dispatch_aggregate(0, &[]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn null_and_zero_rows_skipped() {
        let rows = [
            &[NeutralValue::Null][..],
            &[f(0.0)][..],
            &[f(2.0)][..],
            &[f(2.0)][..],
        ];
        // harmonic mean of {2,2} = 2
        assert_eq!(
            Core::dispatch_aggregate(0, &rows).unwrap(),
            NeutralValue::Float64(2.0)
        );
    }
}
