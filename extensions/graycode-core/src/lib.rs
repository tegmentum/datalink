//! Neutral core for the `graycode` extension — reflected binary Gray code —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `gray_encode(n bigint) -> bigint` — `n ^ (n >> 1)`.
//!   * `gray_decode(g bigint) -> bigint` — the inverse fold.
//!
//! Negative / NULL input -> NULL. The surface is identical in both ports.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// binary -> Gray code.
pub fn encode(n: u64) -> u64 {
    n ^ (n >> 1)
}

/// Gray code -> binary.
pub fn decode(g: u64) -> u64 {
    let mut g = g;
    let mut x = g;
    while g > 0 {
        g >>= 1;
        x ^= g;
    }
    x
}

datalink_extcore::declare! {
    core = Core;
    extension = "graycode";
    version = env!("CARGO_PKG_VERSION");

    scalar gray_encode(int64) -> int64 [propagate, deterministic] = |args| {
        let n = args.arg_int(0, "gray_encode")?;
        Ok(if n < 0 { NeutralValue::Null } else { NeutralValue::Int64(encode(n as u64) as i64) })
    };

    scalar gray_decode(int64) -> int64 [propagate, deterministic] = |args| {
        let g = args.arg_int(0, "gray_decode")?;
        Ok(if g < 0 { NeutralValue::Null } else { NeutralValue::Int64(decode(g as u64) as i64) })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn roundtrip() {
        for n in [0u64, 1, 2, 3, 17, 255, 1024] {
            let g = encode(n);
            assert_eq!(decode(g), n);
        }
    }

    #[test]
    fn dispatch_and_negative_null() {
        assert_eq!(
            Core::dispatch(idx("gray_encode"), &[NeutralValue::Int64(4)]).unwrap(),
            NeutralValue::Int64(6)
        );
        assert_eq!(
            Core::dispatch(idx("gray_decode"), &[NeutralValue::Int64(6)]).unwrap(),
            NeutralValue::Int64(4)
        );
        assert_eq!(
            Core::dispatch(idx("gray_encode"), &[NeutralValue::Int64(-1)]).unwrap(),
            NeutralValue::Null
        );
    }
}
