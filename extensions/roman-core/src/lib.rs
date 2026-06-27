//! Neutral core for the `roman` extension — Roman numeral conversion —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Reconciled drift (name-family union)
//!
//! ducklink shipped `to_roman` / `from_roman` (on the `roman` crate);
//! sqlink shipped `roman_encode` / `roman_decode` / `roman_validate`
//! (hand-rolled: case-insensitive, canonical-form-checked). The names do
//! not collide, so this core exposes BOTH families and each database
//! gains the other's. The two encoders agree on the valid range
//! (1..=3999); `roman_decode` additionally upper-cases its input and
//! rejects non-canonical numerals (e.g. "IIII"), matching sqlink.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic (DB-agnostic). The `dl_*` functions are sqlink's hand-rolled
/// algorithm (ported verbatim); `to_roman`/`from_roman` ride the `roman`
/// crate exactly as ducklink did.
pub mod logic {
    use alloc::string::String;

    const PAIRS: &[(i64, &str)] = &[
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"),
        (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"),
        (5, "V"), (4, "IV"), (1, "I"),
    ];

    fn char_to_val(c: char) -> Option<i64> {
        match c {
            'I' => Some(1), 'V' => Some(5), 'X' => Some(10), 'L' => Some(50),
            'C' => Some(100), 'D' => Some(500), 'M' => Some(1000), _ => None,
        }
    }

    /// sqlink `roman_encode`: standard subtractive form, 1..=3999.
    pub fn encode(mut n: i64) -> Option<String> {
        if !(1..=3999).contains(&n) {
            return None;
        }
        let mut out = String::new();
        for &(v, s) in PAIRS {
            while n >= v {
                out.push_str(s);
                n -= v;
            }
        }
        Some(out)
    }

    /// sqlink `roman_decode`: case-insensitive, canonical-form-checked.
    pub fn decode(s: &str) -> Option<i64> {
        let s = s.trim().to_ascii_uppercase();
        if s.is_empty() {
            return None;
        }
        let mut total: i64 = 0;
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let cur = char_to_val(bytes[i] as char)?;
            let nxt = bytes.get(i + 1).and_then(|c| char_to_val(*c as char));
            if let Some(nx) = nxt {
                if cur < nx {
                    total += nx - cur;
                    i += 2;
                    continue;
                }
            }
            total += cur;
            i += 1;
        }
        if encode(total).as_deref() == Some(s.as_str()) {
            Some(total)
        } else {
            None
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "roman";
    version = env!("CARGO_PKG_VERSION");

    // ducklink family (the `roman` crate) — byte-parity preserved.
    scalar to_roman(int64) -> text [propagate, deterministic] = |args| {
        let n = args.arg_int(0, "to_roman")?;
        Ok(match i32::try_from(n).ok().and_then(roman::to) {
            Some(s) => NeutralValue::Text(alloc::string::String::from(s)),
            None => NeutralValue::Null,
        })
    };
    scalar from_roman(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "from_roman")?;
        Ok(match roman::from(&s) {
            Some(n) => NeutralValue::Int64(n as i64),
            None => NeutralValue::Null,
        })
    };

    // sqlink family (hand-rolled) — the gained superset.
    scalar roman_encode(int64) -> text [propagate, deterministic] = |args| {
        Ok(match logic::encode(args.arg_int(0, "roman_encode")?) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
    scalar roman_decode(text) -> int64 [propagate, deterministic] = |args| {
        Ok(match logic::decode(&args.arg_text(0, "roman_decode")?) {
            Some(n) => NeutralValue::Int64(n),
            None => NeutralValue::Null,
        })
    };
    scalar roman_validate(text) -> boolean [propagate, deterministic] = |args| {
        Ok(NeutralValue::Boolean(logic::decode(&args.arg_text(0, "roman_validate")?).is_some()))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn i(n: i64) -> NeutralValue { NeutralValue::Int64(n) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn ducklink_family() {
        assert_eq!(Core::dispatch(idx("to_roman"), &[i(2024)]).unwrap(), t("MMXXIV"));
        assert_eq!(Core::dispatch(idx("to_roman"), &[i(49)]).unwrap(), t("XLIX"));
        assert_eq!(Core::dispatch(idx("to_roman"), &[i(0)]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("from_roman"), &[t("MCMLXXXIV")]).unwrap(), i(1984));
        assert_eq!(Core::dispatch(idx("from_roman"), &[t("NOPE")]).unwrap(), NeutralValue::Null);
    }

    #[test]
    fn sqlink_family() {
        assert_eq!(Core::dispatch(idx("roman_encode"), &[i(1)]).unwrap(), t("I"));
        assert_eq!(Core::dispatch(idx("roman_encode"), &[i(3999)]).unwrap(), t("MMMCMXCIX"));
        assert_eq!(Core::dispatch(idx("roman_encode"), &[i(4000)]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("roman_decode"), &[t("MCMXCIV")]).unwrap(), i(1994));
        assert_eq!(Core::dispatch(idx("roman_decode"), &[t("iv")]).unwrap(), i(4));
        assert_eq!(Core::dispatch(idx("roman_validate"), &[t("MCMXCIV")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("roman_validate"), &[t("IIII")]).unwrap(), NeutralValue::Boolean(false));
    }
}
