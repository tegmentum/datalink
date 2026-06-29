//! Neutral core for the `baseconv` extension — arbitrary-radix string ->
//! integer, the inverse of DuckDB's built-in `to_base(n, radix)` — written
//! ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `from_base(text, base) -> int64` — base 2..=36, case-insensitive.
//!     NULL / unparseable / out-of-range base -> NULL.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    /// Parse `s` (trimmed) as a base-`base` integer, `None` on any error or
    /// an out-of-range base.
    pub fn from_base(s: &str, base: i64) -> Option<i64> {
        if !(2..=36).contains(&base) {
            return None;
        }
        i64::from_str_radix(s.trim(), base as u32).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "baseconv";
    version = env!("CARGO_PKG_VERSION");

    scalar from_base(text, int64) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "from_base")?;
        let base = args.arg_int(1, "from_base")?;
        Ok(match logic::from_base(&s, base) {
            Some(n) => NeutralValue::Int64(n),
            None => NeutralValue::Null,
        })
    };
}
