//! Neutral core for the `minhash` extension — MinHash set similarity as a
//! DuckDB AGGREGATE build plus a similarity scalar — written ONCE. The
//! per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `minhash(value) -> text`  AGGREGATE: hex of a 64-slot signature
//!     over the group's non-NULL text values. A group with NO usable
//!     values yields NULL.
//!   * `minhash_similarity(a, b) -> double`  estimated Jaccard of the two
//!     signatures. A malformed/NULL signature yields NULL.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic MinHash logic, shared by the aggregate build and the
/// similarity scalar.
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const K: usize = 64;

    pub fn fnv1a(b: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &x in b {
            h ^= x as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    pub fn slot_hash(base: u64, i: usize) -> u32 {
        let a = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1) | 1;
        let b = (i as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        (base.wrapping_mul(a).wrapping_add(b) >> 32) as u32
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

    pub fn sig_to_hex(sig: &[u32; K]) -> String {
        let mut bytes = Vec::with_capacity(K * 4);
        for v in sig {
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        hex_encode(&bytes)
    }

    pub fn hex_to_sig(s: &str) -> Option<[u32; K]> {
        let b = hex_decode(s)?;
        if b.len() != K * 4 {
            return None;
        }
        let mut sig = [0u32; K];
        for (i, slot) in sig.iter_mut().enumerate() {
            *slot = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        Some(sig)
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
    extension = "minhash";
    version = env!("CARGO_PKG_VERSION");

    scalar minhash_similarity(text, text) -> float64 [propagate, deterministic] = |args| {
        let a = match logic::hex_to_sig(&args.arg_text(0, "minhash_similarity")?) {
            Some(s) => s,
            None => return Ok(NeutralValue::Null),
        };
        let b = match logic::hex_to_sig(&args.arg_text(1, "minhash_similarity")?) {
            Some(s) => s,
            None => return Ok(NeutralValue::Null),
        };
        let matches = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
        Ok(NeutralValue::Float64(matches as f64 / logic::K as f64))
    };

    aggregate minhash(text) -> text [deterministic] {
        state = ([u32; logic::K], bool);
        init = ([u32::MAX; logic::K], false);
        step = |st: &mut ([u32; logic::K], bool), row: &[NeutralValue]| {
            if let Some(s) = row.first().and_then(as_text) {
                st.1 = true;
                let base = logic::fnv1a(s.as_bytes());
                for (i, slot) in st.0.iter_mut().enumerate() {
                    let h = logic::slot_hash(base, i);
                    if h < *slot {
                        *slot = h;
                    }
                }
            }
        };
        finalize = |st: ([u32; logic::K], bool)| {
            if !st.1 {
                Ok(NeutralValue::Null)
            } else {
                Ok(NeutralValue::Text(logic::sig_to_hex(&st.0)))
            }
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
        Core::DECLS.iter().position(|d| d.name == "minhash").unwrap()
    }
    fn sidx() -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == "minhash_similarity")
            .unwrap()
    }
    fn sig(items: &[&str]) -> alloc::string::String {
        let rows: alloc::vec::Vec<[NeutralValue; 1]> =
            items.iter().map(|s| [t(s)]).collect();
        let refs: alloc::vec::Vec<&[NeutralValue]> = rows.iter().map(|r| &r[..]).collect();
        match Core::dispatch_aggregate(aidx(), &refs).unwrap() {
            NeutralValue::Text(s) => s,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn identical_sets_are_one() {
        let a = sig(&["a", "b", "c", "d"]);
        let b = sig(&["a", "b", "c", "d"]);
        assert_eq!(
            Core::dispatch(sidx(), &[t(&a), t(&b)]).unwrap(),
            NeutralValue::Float64(1.0)
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
    fn bad_sig_is_null() {
        assert_eq!(
            Core::dispatch(sidx(), &[t("00"), t("ff")]).unwrap(),
            NeutralValue::Null
        );
    }
}
