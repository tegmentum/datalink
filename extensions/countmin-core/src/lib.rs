//! Neutral core for the `countmin` extension — a Count-Min sketch as a
//! DuckDB AGGREGATE build plus a frequency-estimate scalar — written
//! ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `count_min(value) -> text`  AGGREGATE: hex of a d=4 x w=512 counter
//!     table over the group's non-NULL text values. An EMPTY group still
//!     returns a (zeroed) sketch, not NULL.
//!   * `cms_estimate(sketch, item) -> bigint`  estimated frequency (never
//!     an under-count). A malformed/NULL sketch or item yields NULL.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic Count-Min logic, shared by the aggregate build and the
/// estimate scalar.
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const D: usize = 4;
    pub const W: usize = 512;
    pub const CELLS: usize = D * W;

    pub fn fnv1a(b: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &x in b {
            h ^= x as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    pub fn col(base: u64, row: usize) -> usize {
        let a = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1) | 1;
        let b = (row as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        (base.wrapping_mul(a).wrapping_add(b) >> 32) as usize % W
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

fn as_text(v: &NeutralValue) -> Option<&str> {
    match v {
        NeutralValue::Text(s) => Some(s),
        _ => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "countmin";
    version = env!("CARGO_PKG_VERSION");

    scalar cms_estimate(text, text) -> int64 [propagate, deterministic] = |args| {
        let bytes = match logic::hex_decode(&args.arg_text(0, "cms_estimate")?) {
            Some(b) if b.len() == logic::CELLS * 4 => b,
            _ => return Ok(NeutralValue::Null),
        };
        let item = args.arg_text(1, "cms_estimate")?;
        let base = logic::fnv1a(item.as_bytes());
        let mut est = u32::MAX;
        for row in 0..logic::D {
            let idx = row * logic::W + logic::col(base, row);
            let c = u32::from_be_bytes([
                bytes[idx * 4],
                bytes[idx * 4 + 1],
                bytes[idx * 4 + 2],
                bytes[idx * 4 + 3],
            ]);
            if c < est {
                est = c;
            }
        }
        Ok(NeutralValue::Int64(est as i64))
    };

    aggregate count_min(text) -> text [deterministic] {
        state = alloc::vec::Vec<u32>;
        init = alloc::vec![0u32; logic::CELLS];
        step = |st: &mut alloc::vec::Vec<u32>, row: &[NeutralValue]| {
            if let Some(s) = row.first().and_then(as_text) {
                let base = logic::fnv1a(s.as_bytes());
                for r in 0..logic::D {
                    let idx = r * logic::W + logic::col(base, r);
                    st[idx] = st[idx].saturating_add(1);
                }
            }
        };
        finalize = |st: alloc::vec::Vec<u32>| {
            let mut bytes = alloc::vec::Vec::with_capacity(logic::CELLS * 4);
            for c in &st {
                bytes.extend_from_slice(&c.to_be_bytes());
            }
            Ok(NeutralValue::Text(logic::hex_encode(&bytes)))
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
        Core::DECLS.iter().position(|d| d.name == "count_min").unwrap()
    }
    fn sidx() -> usize {
        Core::DECLS.iter().position(|d| d.name == "cms_estimate").unwrap()
    }

    #[test]
    fn counts_then_estimates() {
        let rows = [&[t("a")][..], &[t("a")][..], &[t("a")][..], &[t("b")][..]];
        let sketch = match Core::dispatch_aggregate(aidx(), &rows).unwrap() {
            NeutralValue::Text(s) => s,
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(
            Core::dispatch(sidx(), &[t(&sketch), t("a")]).unwrap(),
            NeutralValue::Int64(3)
        );
        assert_eq!(
            Core::dispatch(sidx(), &[t(&sketch), t("b")]).unwrap(),
            NeutralValue::Int64(1)
        );
        assert_eq!(
            Core::dispatch(sidx(), &[t(&sketch), t("z")]).unwrap(),
            NeutralValue::Int64(0)
        );
    }

    #[test]
    fn bad_sketch_is_null() {
        assert_eq!(
            Core::dispatch(sidx(), &[t("00"), t("x")]).unwrap(),
            NeutralValue::Null
        );
    }
}
