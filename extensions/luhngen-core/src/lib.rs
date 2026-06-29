//! Neutral core for the `luhngen` extension — Luhn check-digit
//! GENERATION — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table. Complements `luhn`
//! (validation).
//!
//!   * `luhn_check_digit(partial) -> int64` — the digit to append.
//!   * `luhn_append(partial) -> text` — partial + check digit.
//!
//! Non-digits are stripped; empty -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::vec::Vec;

    pub fn digits(s: &str) -> Vec<u8> {
        s.chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect()
    }

    /// Check digit so partial+digit passes Luhn (the appended digit sits at
    /// position 1 from the right, so the partial's last digit is doubled).
    pub fn check_digit(ds: &[u8]) -> u8 {
        let mut sum = 0u32;
        let mut double = true;
        for &d in ds.iter().rev() {
            let mut x = d as u32;
            if double {
                x *= 2;
                if x > 9 {
                    x -= 9;
                }
            }
            sum += x;
            double = !double;
        }
        ((10 - sum % 10) % 10) as u8
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "luhngen";
    version = env!("CARGO_PKG_VERSION");

    scalar luhn_check_digit(text) -> int64 [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "luhn_check_digit")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Null);
        }
        Ok(NeutralValue::Int64(logic::check_digit(&ds) as i64))
    };

    scalar luhn_append(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "luhn_append")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Null);
        }
        let cd = logic::check_digit(&ds);
        let body: String = ds.iter().map(|d| (b'0' + d) as char).collect();
        Ok(NeutralValue::Text(alloc::format!("{}{}", body, cd)))
    };
}
