//! Neutral core for the `isin` extension — ISIN (ISO 6166) securities
//! identifier validation + field extraction — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare)
//! table below.
//!
//! # Surface (zero drift)
//!
//! The ducklink and sqlink ports already shipped the identical
//! 4-function surface and the identical expand + Luhn mod-10 algorithm;
//! this core is a straight pull-up (no reconciliation needed).
//!
//!   * `isin_validate(text) -> boolean` — true if the check digit is correct
//!   * `isin_check_digit(text) -> int64` — the expected Luhn check digit (0..9)
//!   * `isin_country(text) -> text` — the 2-letter ISO country prefix
//!   * `isin_nsin(text) -> text` — the 9-char national security identifier

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// ISIN algorithm (DB-agnostic), byte-for-byte the pre-pullup logic.
pub mod logic {
    use alloc::format;
    use alloc::string::String;

    /// Strip whitespace + hyphens and upper-case.
    pub fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect::<String>()
            .to_ascii_uppercase()
    }

    /// Expand each letter to its 2-digit value (A=10..Z=35) and each
    /// digit to itself, concatenated. `None` on any non-alphanumeric.
    pub fn expand(s: &str) -> Option<String> {
        let mut out = String::with_capacity(s.len() * 2);
        for c in s.chars() {
            if c.is_ascii_digit() {
                out.push(c);
            } else if c.is_ascii_alphabetic() {
                let v = (c.to_ascii_uppercase() as u32) - ('A' as u32) + 10;
                out.push_str(&format!("{v}"));
            } else {
                return None;
            }
        }
        Some(out)
    }

    /// Luhn check digit (0..9) over a digit-only string.
    pub fn luhn_check_digit(s: &str) -> Option<u32> {
        let mut sum = 0u32;
        let mut alt = true;
        for c in s.chars().rev() {
            let d = c.to_digit(10)?;
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
        Some((10 - (sum % 10)) % 10)
    }

    /// Expected check digit for a normalized ISIN (12 chars).
    pub fn expected_check_digit(normalized: &str) -> Option<u32> {
        if normalized.len() != 12 {
            return None;
        }
        expand(&normalized[..11]).as_deref().and_then(luhn_check_digit)
    }

    /// True if the normalized ISIN's trailing check digit is correct.
    pub fn validate(normalized: &str) -> bool {
        if normalized.len() != 12 {
            return false;
        }
        let last = match normalized.as_bytes()[11] {
            b @ b'0'..=b'9' => (b - b'0') as u32,
            _ => return false,
        };
        matches!(expected_check_digit(normalized), Some(expected) if expected == last)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "isin";
    version = env!("CARGO_PKG_VERSION");

    scalar isin_validate(text) -> boolean [propagate, deterministic] = |args| {
        let n = logic::normalize(&args.arg_text(0, "isin_validate")?);
        Ok(NeutralValue::Boolean(logic::validate(&n)))
    };
    scalar isin_check_digit(text) -> int64 [propagate, deterministic] = |args| {
        let n = logic::normalize(&args.arg_text(0, "isin_check_digit")?);
        Ok(match logic::expected_check_digit(&n) {
            Some(d) => NeutralValue::Int64(d as i64),
            None => NeutralValue::Null,
        })
    };
    scalar isin_country(text) -> text [propagate, deterministic] = |args| {
        let n = logic::normalize(&args.arg_text(0, "isin_country")?);
        Ok(if n.len() == 12 {
            NeutralValue::Text(alloc::string::String::from(&n[..2]))
        } else {
            NeutralValue::Null
        })
    };
    scalar isin_nsin(text) -> text [propagate, deterministic] = |args| {
        let n = logic::normalize(&args.arg_text(0, "isin_nsin")?);
        Ok(if n.len() == 12 {
            NeutralValue::Text(alloc::string::String::from(&n[2..11]))
        } else {
            NeutralValue::Null
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("isin_validate"), &[t("US0378331005")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("isin_validate"), &[t("US0378331006")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("isin_check_digit"), &[t("US0378331005")]).unwrap(), NeutralValue::Int64(5));
        assert_eq!(Core::dispatch(idx("isin_country"), &[t("US0378331005")]).unwrap(), t("US"));
        assert_eq!(Core::dispatch(idx("isin_nsin"), &[t("US0378331005")]).unwrap(), t("037833100"));
        assert_eq!(Core::dispatch(idx("isin_check_digit"), &[t("junk")]).unwrap(), NeutralValue::Null);
    }
}
