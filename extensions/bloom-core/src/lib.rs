//! Neutral core for the `bloom` extension — a Bloom filter as a DuckDB
//! AGGREGATE build plus a membership scalar — written ONCE. The per-DB
//! shim is generated from the [`declare!`](datalink_extcore::declare)
//! table below.
//!
//! # Functions
//!
//!   * `bloom_filter(value) -> text`  AGGREGATE: hex of an 8192-bit, k=5
//!     filter over the group's non-NULL text values. An EMPTY group still
//!     returns a (zeroed) filter, not NULL.
//!   * `bloom_contains(filter_hex, item) -> boolean`  probabilistic
//!     membership; never a false negative. A malformed/NULL filter or
//!     item yields NULL.
//!
//! The aggregate fold runs entirely in-guest (the duckdb host buffers the
//! group), so the bit-array state is a native value and never marshals.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic Bloom-filter logic, shared by the aggregate build and the
/// membership scalar (so the two can never drift).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const M: usize = 8192; // bits
    pub const BYTES: usize = M / 8; // 1024
    pub const K: usize = 5; // hash functions

    pub fn fnv1a(b: &[u8], basis: u64) -> u64 {
        let mut h = basis;
        for &x in b {
            h ^= x as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    pub fn positions(item: &str) -> [usize; K] {
        let h1 = fnv1a(item.as_bytes(), 0xcbf2_9ce4_8422_2325);
        let h2 = fnv1a(item.as_bytes(), 0x8422_2325_cbf2_9ce4) | 1;
        let mut p = [0usize; K];
        for (i, slot) in p.iter_mut().enumerate() {
            *slot = (h1.wrapping_add((i as u64).wrapping_mul(h2)) % M as u64) as usize;
        }
        p
    }

    const HEX: &[u8] = b"0123456789abcdef";

    pub fn hex_encode(b: &[u8]) -> String {
        let mut o = String::with_capacity(b.len() * 2);
        for &x in b {
            o.push(HEX[(x >> 4) as usize] as char);
            o.push(HEX[(x & 0xf) as usize] as char);
        }
        o
    }

    pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
        let s = s.trim();
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect()
    }
}

/// Borrow a text value, skipping NULL/other arms (the aggregate's per-row
/// NULL skip — matches the hand-written `and_then(text)`).
fn as_text(v: &NeutralValue) -> Option<&str> {
    match v {
        NeutralValue::Text(s) => Some(s),
        _ => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "bloom";
    version = env!("CARGO_PKG_VERSION");

    scalar bloom_contains(text, text) -> boolean [propagate, deterministic] = |args| {
        let filter = match logic::hex_decode(&args.arg_text(0, "bloom_contains")?) {
            Some(b) if b.len() == logic::BYTES => b,
            _ => return Ok(NeutralValue::Null),
        };
        let item = args.arg_text(1, "bloom_contains")?;
        let present = logic::positions(&item)
            .iter()
            .all(|&p| filter[p / 8] & (1 << (p % 8)) != 0);
        Ok(NeutralValue::Boolean(present))
    };

    aggregate bloom_filter(text) -> text [deterministic] {
        state = alloc::vec::Vec<u8>;
        init = alloc::vec![0u8; logic::BYTES];
        step = |st: &mut alloc::vec::Vec<u8>, row: &[NeutralValue]| {
            if let Some(s) = row.first().and_then(as_text) {
                for &p in logic::positions(s).iter() {
                    st[p / 8] |= 1 << (p % 8);
                }
            }
        };
        finalize = |st: alloc::vec::Vec<u8>| {
            Ok(NeutralValue::Text(logic::hex_encode(&st)))
        };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(alloc::string::String::from(s))
    }
    fn aidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "bloom_filter").unwrap()
    }
    fn sidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "bloom_contains").unwrap()
    }

    #[test]
    fn build_then_contains() {
        let rows = [&[t("apple")][..], &[t("banana")][..], &[t("cherry")][..]];
        let filter = Core::dispatch_aggregate(aidx(), &rows).unwrap();
        let hex = match filter {
            NeutralValue::Text(s) => s,
            other => panic!("expected text filter, got {other:?}"),
        };
        assert_eq!(
            Core::dispatch(sidx(), &[t(&hex), t("apple")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(sidx(), &[t(&hex), t("durian")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }

    #[test]
    fn empty_group_is_zeroed_not_null() {
        // Empty group -> a zeroed (all-bits-clear) filter, NOT NULL.
        let filter = Core::dispatch_aggregate(aidx(), &[]).unwrap();
        match filter {
            NeutralValue::Text(s) => assert_eq!(s, logic::hex_encode(&[0u8; logic::BYTES])),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn bad_filter_is_null() {
        assert_eq!(
            Core::dispatch(sidx(), &[t("00"), t("x")]).unwrap(),
            NeutralValue::Null
        );
    }
}
