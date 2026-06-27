//! Neutral core for the `tdigest` extension — t-digest quantile
//! estimation as a DuckDB AGGREGATE build plus quantile/count query
//! scalars — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `tdigest(value) -> blob`  AGGREGATE: a serialized t-digest sketch
//!     over the group's non-NULL, finite doubles. An EMPTY group yields
//!     NULL.
//!   * `tdigest_quantile(digest, q) -> double`  estimate the q-quantile
//!     (q in [0,1]). A malformed/NULL digest, out-of-range q, empty
//!     digest, or non-finite estimate yields NULL.
//!   * `tdigest_count(digest) -> bigint`  total count of values in the
//!     sketch. A malformed/NULL digest yields NULL.
//!
//! Not `no_std`: `bincode`/`tdigest` pull in `std`. The accumulator state
//! is a `Vec<f64>`; the sketch only materializes at `finalize`, and (the
//! duckdb host buffering the group) the whole fold runs in one call so the
//! state never marshals across the boundary.

extern crate alloc;

use datalink_extcore::NeutralValue;
use tdigest::TDigest;

/// Borrow a numeric value as `f64`, accepting the neutral arms DuckDB can
/// produce for a `DOUBLE`-typed argument; NULL/other yields `None`.
fn as_f64(v: &NeutralValue) -> Option<f64> {
    match v {
        NeutralValue::Float64(f) => Some(*f),
        NeutralValue::Int64(i) => Some(*i as f64),
        _ => None,
    }
}

/// Deserialize a t-digest from blob bytes, returning None on any
/// malformed input.
fn decode(bytes: &[u8]) -> Option<TDigest> {
    bincode::deserialize::<TDigest>(bytes).ok()
}

datalink_extcore::declare! {
    core = Core;
    extension = "tdigest";
    version = env!("CARGO_PKG_VERSION");

    scalar tdigest_quantile(blob, float64) -> float64 [propagate, deterministic] = |args| {
        let digest = match decode(&args.arg_blob(0, "tdigest_quantile")?) {
            Some(d) => d,
            None => return Ok(NeutralValue::Null),
        };
        let q = args.arg_float(1, "tdigest_quantile")?;
        if !(0.0..=1.0).contains(&q) {
            return Ok(NeutralValue::Null);
        }
        if digest.is_empty() {
            return Ok(NeutralValue::Null);
        }
        let est = digest.estimate_quantile(q);
        if est.is_finite() {
            Ok(NeutralValue::Float64(est))
        } else {
            Ok(NeutralValue::Null)
        }
    };

    scalar tdigest_count(blob) -> int64 [propagate, deterministic] = |args| {
        let digest = match decode(&args.arg_blob(0, "tdigest_count")?) {
            Some(d) => d,
            None => return Ok(NeutralValue::Null),
        };
        Ok(NeutralValue::Int64(digest.count() as i64))
    };

    aggregate tdigest(float64) -> blob [deterministic] {
        state = alloc::vec::Vec<f64>;
        init = alloc::vec::Vec::new();
        step = |st: &mut alloc::vec::Vec<f64>, row: &[NeutralValue]| {
            if let Some(x) = row.first().and_then(as_f64) {
                if x.is_finite() {
                    st.push(x);
                }
            }
        };
        finalize = |values: alloc::vec::Vec<f64>| {
            if values.is_empty() {
                return Ok(NeutralValue::Null);
            }
            let digest = TDigest::new_with_size(100).merge_unsorted(values);
            let bytes = bincode::serialize(&digest)
                .map_err(|e| alloc::format!("tdigest serialize failed: {e}"))?;
            Ok(NeutralValue::Blob(bytes))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn f(x: f64) -> NeutralValue {
        NeutralValue::Float64(x)
    }
    fn aidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "tdigest").unwrap()
    }
    fn qidx() -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == "tdigest_quantile")
            .unwrap()
    }
    fn cidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "tdigest_count").unwrap()
    }

    #[test]
    fn build_then_query() {
        let rows: alloc::vec::Vec<[NeutralValue; 1]> =
            (1..=100).map(|n| [f(n as f64)]).collect();
        let refs: alloc::vec::Vec<&[NeutralValue]> = rows.iter().map(|r| &r[..]).collect();
        let blob = match Core::dispatch_aggregate(aidx(), &refs).unwrap() {
            NeutralValue::Blob(b) => b,
            other => panic!("expected blob, got {other:?}"),
        };
        let b = NeutralValue::Blob(blob);
        match Core::dispatch(qidx(), &[b.clone(), f(0.5)]).unwrap() {
            NeutralValue::Float64(v) => assert!((v - 50.0).abs() < 2.0, "median {v}"),
            other => panic!("expected float, got {other:?}"),
        }
        assert_eq!(
            Core::dispatch(cidx(), &[b]).unwrap(),
            NeutralValue::Int64(100)
        );
    }

    #[test]
    fn empty_group_is_null() {
        assert_eq!(
            Core::dispatch_aggregate(aidx(), &[]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn bad_digest_is_null() {
        assert_eq!(
            Core::dispatch(qidx(), &[NeutralValue::Blob(b"00".to_vec()), f(0.5)]).unwrap(),
            NeutralValue::Null
        );
    }
}
