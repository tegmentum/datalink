//! Neutral core for the `luhn` extension — Luhn (mod-10) checksum —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `luhn_validate(text) -> boolean` — the digit string passes Luhn.
//!   * `luhn_check_digit(text) -> int64` — the check digit (0..9) for the
//!     body; empty / non-digit input -> NULL.
//!
//! Whitespace and hyphens are ignored; any other non-digit makes the
//! input invalid.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::vec::Vec;

    pub fn digits(s: &str) -> Option<Vec<u32>> {
        let mut out = Vec::with_capacity(s.len());
        for c in s.chars() {
            if c.is_whitespace() || c == '-' {
                continue;
            }
            out.push(c.to_digit(10)?);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub fn check_digit(body: &[u32]) -> u32 {
        let mut sum = 0u32;
        let mut alt = true;
        for &d in body.iter().rev() {
            let v = if alt {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            };
            sum += v;
            alt = !alt;
        }
        (10 - (sum % 10)) % 10
    }

    pub fn validate(num: &[u32]) -> bool {
        num.len() >= 2 && check_digit(&num[..num.len() - 1]) == num[num.len() - 1]
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "luhn";
    version = env!("CARGO_PKG_VERSION");

    scalar luhn_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "luhn_validate")?;
        let ok = logic::digits(&raw).map(|d| logic::validate(&d)).unwrap_or(false);
        Ok(NeutralValue::Boolean(ok))
    };

    scalar luhn_check_digit(text) -> int64 [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "luhn_check_digit")?;
        Ok(match logic::digits(&raw) {
            Some(d) => NeutralValue::Int64(logic::check_digit(&d) as i64),
            None => NeutralValue::Null,
        })
    };
}
