//! Neutral core for the `stats` statistical-AGGREGATE pack — written
//! ONCE. The per-DB aggregate shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the percentile aggregates DuckDB lacks as plain names
//!
//! DuckDB ships a rich aggregate set as BUILTINS: `stddev`/`stddev_pop`/
//! `stddev_samp`/`variance`/`var_pop`/`var_samp`/`median`/`mode`/`corr`/
//! `covar_pop`/`covar_samp`/`skewness`/`kurtosis`/`any_value`/`bit_and`/
//! `bit_or`/`bit_xor`/`array_agg`/`string_agg`/the `regr_*` family.
//! Re-registering any of those (same name + arity) would collide, so they
//! are deliberately NOT declared here.
//!
//! What remains — and is what ducklink GAINS — are the percentile
//! aggregates DuckDB has no plain-catalog form for: `percentile(v, p)`,
//! `percentile_cont(v, p)`, `percentile_disc(v, p)`, all taking the
//! percentile `p` in **0..100** (matching sqlink). DuckDB's own
//! `percentile_cont`/`percentile_disc` are ordered-set aggregates spelled
//! `... WITHIN GROUP (ORDER BY v)` (or `quantile_cont(v, 0.5)`), so the
//! 2-arg names declared here do not exist in DuckDB's catalog. The
//! algorithms are byte-identical to sqlink's `stats` (the `Samples`
//! aggregator), so a future `sqlite_shim!` over this core reproduces
//! sqlink's behaviour.

extern crate alloc;

use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Coerce a neutral value to `f64`, accepting the arms DuckDB can produce
/// for a `DOUBLE`-typed aggregate arg (the host coerces the column to the
/// registered `Float64`; an integer literal/column widens). TEXT is parsed
/// as a fallback (matching sqlink's `to_f64`). Anything else (incl. NULL)
/// yields `None`, so `step` skips it.
fn num(v: &NeutralValue) -> Option<f64> {
    match v {
        NeutralValue::Float64(x) => Some(*x),
        NeutralValue::Int64(x) => Some(*x as f64),
        NeutralValue::Text(s) => s.parse().ok(),
        _ => None,
    }
}

/// Running accumulator for one percentile aggregation: the percentile `p`
/// (taken from the first row that supplies it) plus the collected values.
#[derive(Default)]
pub struct PercAcc {
    pub p: Option<f64>,
    pub values: Vec<f64>,
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    s
}

/// Linear-interpolation percentile (SQL `PERCENTILE_CONT`). `p` in 0..100.
/// Byte-identical to sqlink's `Samples::percentile`.
pub fn percentile_cont(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let s = sorted(values);
    let n = s.len();
    if n == 1 {
        return Some(s[0]);
    }
    let rank = (n as f64 - 1.0) * (p / 100.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        Some(s[lo])
    } else {
        let frac = rank - lo as f64;
        Some(s[lo] * (1.0 - frac) + s[hi] * frac)
    }
}

/// Discrete percentile (SQL `PERCENTILE_DISC`). `p` in 0..100.
/// Byte-identical to sqlink's `Samples::percentile_disc`.
pub fn percentile_disc(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let s = sorted(values);
    let n = s.len();
    let idx = ((p / 100.0 * n as f64).ceil() as isize - 1).clamp(0, n as isize - 1) as usize;
    Some(s[idx])
}

/// Fold one row into a percentile accumulator: skip NULL/non-numeric
/// values (SQL aggregate convention); the first row that supplies a
/// numeric `p` wins (the row-invariant assumption sqlink makes).
fn perc_step(st: &mut PercAcc, row: &[NeutralValue]) {
    let x = match row.first().and_then(num) {
        Some(x) => x,
        None => return,
    };
    if st.p.is_none() {
        if let Some(p) = row.get(1).and_then(num) {
            st.p = Some(p);
        }
    }
    st.values.push(x);
}

datalink_extcore::declare! {
    core = Core;
    extension = "stats";
    version = env!("CARGO_PKG_VERSION");

    // percentile(value, p) — sqlink's continuous percentile; p in 0..100.
    aggregate percentile(float64, float64) -> float64 [deterministic] {
        state = PercAcc;
        init = PercAcc::default();
        step = perc_step;
        finalize = |st: PercAcc| {
            Ok(percentile_cont(&st.values, st.p.unwrap_or(50.0))
                .map(NeutralValue::Float64)
                .unwrap_or(NeutralValue::Null))
        };
    }

    // percentile_cont(value, p) — linear interpolation; p in 0..100.
    aggregate percentile_cont(float64, float64) -> float64 [deterministic] {
        state = PercAcc;
        init = PercAcc::default();
        step = perc_step;
        finalize = |st: PercAcc| {
            Ok(percentile_cont(&st.values, st.p.unwrap_or(50.0))
                .map(NeutralValue::Float64)
                .unwrap_or(NeutralValue::Null))
        };
    }

    // percentile_disc(value, p) — nearest actual sample; p in 0..100.
    aggregate percentile_disc(float64, float64) -> float64 [deterministic] {
        state = PercAcc;
        init = PercAcc::default();
        step = perc_step;
        finalize = |st: PercAcc| {
            Ok(percentile_disc(&st.values, st.p.unwrap_or(50.0))
                .map(NeutralValue::Float64)
                .unwrap_or(NeutralValue::Null))
        };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(n: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == n).unwrap()
    }
    fn agg(n: &str, rows: &[(f64, f64)]) -> NeutralValue {
        let owned: std::vec::Vec<std::vec::Vec<NeutralValue>> = rows
            .iter()
            .map(|(v, p)| std::vec![NeutralValue::Float64(*v), NeutralValue::Float64(*p)])
            .collect();
        let refs: std::vec::Vec<&[NeutralValue]> = owned.iter().map(|r| r.as_slice()).collect();
        Core::dispatch_aggregate(idx(n), &refs).unwrap()
    }
    fn f(v: NeutralValue) -> f64 {
        match v {
            NeutralValue::Float64(x) => x,
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn cont_over_1_to_5() {
        // {1,2,3,4,5}, p=50 -> 3.0; p=25 -> 2.0; p=75 -> 4.0.
        let d = [(1.0, 50.0), (2.0, 50.0), (3.0, 50.0), (4.0, 50.0), (5.0, 50.0)];
        assert!((f(agg("percentile_cont", &d)) - 3.0).abs() < 1e-9);
        assert!((f(agg("percentile", &d)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn cont_vs_disc_even() {
        // {1,2,3,4}, p=50: cont interpolates to 2.5, disc picks 2.0.
        let d = [(1.0, 50.0), (2.0, 50.0), (3.0, 50.0), (4.0, 50.0)];
        assert!((f(agg("percentile_cont", &d)) - 2.5).abs() < 1e-9);
        assert!((f(agg("percentile_disc", &d)) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn empty_group_is_null() {
        assert_eq!(
            Core::dispatch_aggregate(idx("percentile"), &[]).unwrap(),
            NeutralValue::Null
        );
    }
}
