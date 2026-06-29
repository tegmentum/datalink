//! Neutral core for the `numfmt` extension — number formatting (comma
//! thousands-grouping + SI/metric prefixes) — written ONCE. The per-DB
//! shim is generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `num_group(value float64, decimals int64) -> text`
//!   * `num_si(value float64) -> text`
//!
//! NULL / non-finite input -> NULL (the non-finite case returns NULL from
//! the body; NULL args propagate). Never panics.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::format;
    use alloc::string::{String, ToString};

    /// Insert commas as thousands separators into the (already-sign-stripped)
    /// integer part string, e.g. "1234567" -> "1,234,567".
    fn group_int(int_part: &str) -> String {
        let bytes = int_part.as_bytes();
        let n = bytes.len();
        let mut out = String::with_capacity(n + n / 3);
        for (idx, &b) in bytes.iter().enumerate() {
            if idx > 0 && (n - idx) % 3 == 0 {
                out.push(',');
            }
            out.push(b as char);
        }
        out
    }

    fn frac_has_nonzero(frac: Option<&str>) -> bool {
        frac.map_or(false, |f| f.bytes().any(|b| b != b'0'))
    }

    /// num_group: fixed `decimals` places, comma thousands-grouping.
    pub fn num_group(value: f64, decimals: i64) -> Option<String> {
        if !value.is_finite() {
            return None;
        }
        let dec = decimals.clamp(0, 30) as usize;
        let formatted = format!("{:.*}", dec, value.abs());
        let (int_part, frac_part) = match formatted.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (formatted.as_str(), None),
        };
        let mut out = String::new();
        if value.is_sign_negative()
            && (int_part.bytes().any(|b| b != b'0') || frac_has_nonzero(frac_part))
        {
            out.push('-');
        }
        out.push_str(&group_int(int_part));
        if let Some(f) = frac_part {
            out.push('.');
            out.push_str(f);
        }
        Some(out)
    }

    /// num_si: metric/SI prefix at 3 significant figures.
    pub fn num_si(value: f64) -> Option<String> {
        if !value.is_finite() {
            return None;
        }
        if value == 0.0 {
            return Some("0".into());
        }
        const POS: [&str; 9] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y"];
        const NEG: [&str; 9] = ["", "m", "u", "n", "p", "f", "a", "z", "y"];

        let neg = value < 0.0;
        let mut mag = value.abs();
        let mut exp: i32 = 0;
        if mag >= 1000.0 {
            while mag >= 1000.0 && exp < (POS.len() as i32 - 1) {
                mag /= 1000.0;
                exp += 1;
            }
        } else if mag < 1.0 {
            while mag < 1.0 && exp > -(NEG.len() as i32 - 1) {
                mag *= 1000.0;
                exp -= 1;
            }
        }

        let decimals = if mag >= 100.0 {
            0
        } else if mag >= 10.0 {
            1
        } else {
            2
        };
        let mut mant = format!("{:.*}", decimals, mag);
        if mant.starts_with("1000") && exp < (POS.len() as i32 - 1) {
            mag /= 1000.0;
            exp += 1;
            mant = format!("{:.2}", mag);
        }
        if mant.contains('.') {
            let trimmed = mant.trim_end_matches('0').trim_end_matches('.');
            mant = trimmed.to_string();
        }

        let prefix = if exp >= 0 {
            POS[exp as usize]
        } else {
            NEG[(-exp) as usize]
        };
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        out.push_str(&mant);
        out.push_str(prefix);
        Some(out)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "numfmt";
    version = env!("CARGO_PKG_VERSION");

    scalar num_group(float64, int64) -> text [propagate, deterministic] = |args| {
        let value = args.arg_float(0, "num_group")?;
        let decimals = args.arg_int(1, "num_group")?;
        Ok(match logic::num_group(value, decimals) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar num_si(float64) -> text [propagate, deterministic] = |args| {
        let value = args.arg_float(0, "num_si")?;
        Ok(match logic::num_si(value) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;
    use std::vec;

    #[test]
    fn group_and_si() {
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Float64(1234567.5), NeutralValue::Int64(2)]).unwrap(),
            NeutralValue::Text(String::from("1,234,567.50"))
        );
        assert_eq!(
            Core::dispatch(1, &[NeutralValue::Float64(1500.0)]).unwrap(),
            NeutralValue::Text(String::from("1.5k"))
        );
    }
}
