//! Neutral core for the `bitfilters` extension — an Xor
//! approximate-membership filter as a DuckDB AGGREGATE build plus a
//! membership scalar — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `xor_filter(value) -> blob`  AGGREGATE: a serialized `Xor8` filter
//!     over the group's distinct non-NULL BIGINT keys (no false
//!     negatives). An EMPTY group yields NULL.
//!   * `xor_filter_contains(filter, value) -> boolean`  probabilistic
//!     membership (~0.4% false-positive, never a false negative). A
//!     malformed/NULL filter or value yields NULL.
//!
//! Not `no_std`: `bincode`/`xorf` pull in `std`. The accumulator state is
//! a `Vec<u64>` of keys; the serialized filter only materializes at
//! `finalize`, and (the duckdb host buffering the group) the whole fold
//! runs in one call so the state never marshals across the boundary.

extern crate alloc;

use datalink_extcore::NeutralValue;
use std::convert::TryFrom;
use xorf::{Filter, Xor8};

/// Borrow a BIGINT key as `u64` (reinterpreting the bits, matching the
/// hand-written `int64`: the same value reinterprets back identically on
/// the query side). The host coerces to the registered `Int64`; NULL/
/// other arms yield `None` (the aggregate's per-row skip).
fn as_u64(v: &NeutralValue) -> Option<u64> {
    match v {
        NeutralValue::Int64(i) => Some(*i as u64),
        _ => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "bitfilters";
    version = env!("CARGO_PKG_VERSION");

    scalar xor_filter_contains(blob, int64) -> boolean [propagate, deterministic] = |args| {
        let bytes = args.arg_blob(0, "xor_filter_contains")?;
        let filter: Xor8 = match bincode::deserialize(&bytes) {
            Ok(f) => f,
            Err(_) => return Ok(NeutralValue::Null),
        };
        let key = args.arg_int(1, "xor_filter_contains")? as u64;
        Ok(NeutralValue::Boolean(filter.contains(&key)))
    };

    aggregate xor_filter(int64) -> blob [deterministic] {
        state = alloc::vec::Vec<u64>;
        init = alloc::vec::Vec::new();
        step = |st: &mut alloc::vec::Vec<u64>, row: &[NeutralValue]| {
            if let Some(k) = row.first().and_then(as_u64) {
                st.push(k);
            }
        };
        finalize = |mut keys: alloc::vec::Vec<u64>| {
            // Xor8 construction requires DISTINCT keys -> sort + dedup.
            keys.sort_unstable();
            keys.dedup();
            if keys.is_empty() {
                return Ok(NeutralValue::Null);
            }
            let filter = Xor8::try_from(&keys)
                .map_err(|e| alloc::format!("xor filter build failed: {e:?}"))?;
            let bytes = bincode::serialize(&filter)
                .map_err(|e| alloc::format!("xor filter serialize failed: {e}"))?;
            Ok(NeutralValue::Blob(bytes))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn i(n: i64) -> NeutralValue {
        NeutralValue::Int64(n)
    }
    fn aidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "xor_filter").unwrap()
    }
    fn sidx() -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == "xor_filter_contains")
            .unwrap()
    }

    #[test]
    fn build_then_contains() {
        let rows = [&[i(10)][..], &[i(20)][..], &[i(30)][..], &[i(40)][..], &[i(50)][..]];
        let blob = match Core::dispatch_aggregate(aidx(), &rows).unwrap() {
            NeutralValue::Blob(b) => b,
            other => panic!("expected blob, got {other:?}"),
        };
        let b = NeutralValue::Blob(blob);
        assert_eq!(
            Core::dispatch(sidx(), &[b.clone(), i(10)]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(sidx(), &[b, i(999999)]).unwrap(),
            NeutralValue::Boolean(false)
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
    fn bad_filter_is_null() {
        assert_eq!(
            Core::dispatch(sidx(), &[NeutralValue::Blob(b"00".to_vec()), i(10)]).unwrap(),
            NeutralValue::Null
        );
    }
}
