//! Neutral core for the `lindel` extension — space-filling curve
//! linearization (Morton/Z-order + Hilbert) — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `morton_encode(x, y) -> int64`, `morton_decode_x/y(z) -> int64`
//!   * `hilbert_encode(x, y) -> int64`, `hilbert_decode_x/y(h) -> int64`
//!
//! Components are the low 32 bits of each input (0 .. 2^32-1) -> a 64-bit
//! index. Negative or out-of-range input -> NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup bit math (DB-agnostic).
pub mod logic {
    pub const MAX_COORD: i64 = 0xFFFF_FFFF;
    const HILBERT_BITS: u32 = 32;

    fn part1by1(n: u64) -> u64 {
        let mut x = n & 0x0000_0000_FFFF_FFFF;
        x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
        x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
        x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        x = (x | (x << 2)) & 0x3333_3333_3333_3333;
        x = (x | (x << 1)) & 0x5555_5555_5555_5555;
        x
    }

    fn compact1by1(n: u64) -> u64 {
        let mut x = n & 0x5555_5555_5555_5555;
        x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
        x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
        x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
        x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
        x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
        x
    }

    pub fn morton_encode(x: u64, y: u64) -> u64 {
        part1by1(x) | (part1by1(y) << 1)
    }

    pub fn morton_decode(z: u64) -> (u64, u64) {
        (compact1by1(z), compact1by1(z >> 1))
    }

    pub fn hilbert_encode(x: u64, y: u64) -> u64 {
        let mut rx: u64;
        let mut ry: u64;
        let mut x = x;
        let mut y = y;
        let mut d: u64 = 0;
        let mut s: u64 = 1u64 << (HILBERT_BITS - 1);
        while s > 0 {
            rx = if (x & s) > 0 { 1 } else { 0 };
            ry = if (y & s) > 0 { 1 } else { 0 };
            d = d.wrapping_add(s.wrapping_mul(s).wrapping_mul((3 * rx) ^ ry));
            if ry == 0 {
                if rx == 1 {
                    x = s.wrapping_sub(1).wrapping_sub(x);
                    y = s.wrapping_sub(1).wrapping_sub(y);
                }
                core::mem::swap(&mut x, &mut y);
            }
            s >>= 1;
        }
        d
    }

    pub fn hilbert_decode(h: u64) -> (u64, u64) {
        let mut rx: u64;
        let mut ry: u64;
        let mut t: u64 = h;
        let mut x: u64 = 0;
        let mut y: u64 = 0;
        let mut s: u64 = 1;
        let n: u64 = 1u64 << HILBERT_BITS;
        while s < n {
            rx = 1 & (t / 2);
            ry = 1 & (t ^ rx);
            if ry == 0 {
                if rx == 1 {
                    x = s.wrapping_sub(1).wrapping_sub(x);
                    y = s.wrapping_sub(1).wrapping_sub(y);
                }
                core::mem::swap(&mut x, &mut y);
            }
            x = x.wrapping_add(s.wrapping_mul(rx));
            y = y.wrapping_add(s.wrapping_mul(ry));
            t /= 4;
            s <<= 1;
        }
        (x, y)
    }

    /// Validate a coordinate component is in [0, 2^32-1]; else None (-> NULL).
    pub fn checked_coord(v: i64) -> Option<u64> {
        if (0..=MAX_COORD).contains(&v) {
            Some(v as u64)
        } else {
            None
        }
    }

    /// Validate an index value is non-negative.
    pub fn checked_index(v: i64) -> Option<u64> {
        if v >= 0 {
            Some(v as u64)
        } else {
            None
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "lindel";
    version = env!("CARGO_PKG_VERSION");

    scalar morton_encode(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let x = args.arg_int(0, "morton_encode")?;
        let y = args.arg_int(1, "morton_encode")?;
        Ok(match (logic::checked_coord(x), logic::checked_coord(y)) {
            (Some(x), Some(y)) => NeutralValue::Int64(logic::morton_encode(x, y) as i64),
            _ => NeutralValue::Null,
        })
    };

    scalar hilbert_encode(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let x = args.arg_int(0, "hilbert_encode")?;
        let y = args.arg_int(1, "hilbert_encode")?;
        Ok(match (logic::checked_coord(x), logic::checked_coord(y)) {
            (Some(x), Some(y)) => NeutralValue::Int64(logic::hilbert_encode(x, y) as i64),
            _ => NeutralValue::Null,
        })
    };

    scalar morton_decode_x(int64) -> int64 [propagate, deterministic] = |args| {
        let z = args.arg_int(0, "morton_decode_x")?;
        Ok(match logic::checked_index(z) {
            Some(z) => NeutralValue::Int64(logic::morton_decode(z).0 as i64),
            None => NeutralValue::Null,
        })
    };

    scalar morton_decode_y(int64) -> int64 [propagate, deterministic] = |args| {
        let z = args.arg_int(0, "morton_decode_y")?;
        Ok(match logic::checked_index(z) {
            Some(z) => NeutralValue::Int64(logic::morton_decode(z).1 as i64),
            None => NeutralValue::Null,
        })
    };

    scalar hilbert_decode_x(int64) -> int64 [propagate, deterministic] = |args| {
        let h = args.arg_int(0, "hilbert_decode_x")?;
        Ok(match logic::checked_index(h) {
            Some(h) => NeutralValue::Int64(logic::hilbert_decode(h).0 as i64),
            None => NeutralValue::Null,
        })
    };

    scalar hilbert_decode_y(int64) -> int64 [propagate, deterministic] = |args| {
        let h = args.arg_int(0, "hilbert_decode_y")?;
        Ok(match logic::checked_index(h) {
            Some(h) => NeutralValue::Int64(logic::hilbert_decode(h).1 as i64),
            None => NeutralValue::Null,
        })
    };
}
