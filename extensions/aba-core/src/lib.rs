//! Neutral core for the `aba` extension — US ABA / bank routing-number
//! checks — written ONCE. The per-DB shims (ducklink `duckdb:extension`,
//! sqlink `sqlite:extension`, sqlink embed) are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Reconciled drift
//!
//! Before the pull-up the surface had drifted: ducklink shipped only
//! `aba_validate`, sqlink shipped `aba_validate` + `aba_frb_district` +
//! `aba_fed_region`. This core adopts sqlink's 3-function SUPERSET, so
//! BOTH databases gain the full surface and the surface can never drift
//! again (both read this one declaration).
//!
//! # Functions
//!
//!   * `aba_validate(text) -> boolean` — 9-digit weighted checksum.
//!   * `aba_frb_district(text) -> int64` — Federal Reserve district
//!     (1-12, 0 for U.S. Treasury), NULL for an unrecognized prefix.
//!   * `aba_fed_region(text) -> text` — the district's region name, NULL
//!     for an unrecognized prefix.

#![no_std]

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Collect digits, ignoring whitespace and hyphens; `None` if any
    /// other non-digit char appears or the result is empty. (ducklink's
    /// strict `digits()` — preserved for `aba_validate` parity.)
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

    /// ABA RTN check: `sum(weight*digit) mod 10 == 0`, weights
    /// 3,7,1,3,7,1,3,7,1 left-to-right; exactly 9 digits.
    pub fn validate_digits(num: &[u32]) -> bool {
        if num.len() != 9 {
            return false;
        }
        let w = [3u32, 7, 1, 3, 7, 1, 3, 7, 1];
        let sum: u32 = num.iter().zip(w).map(|(&d, k)| d * k).sum();
        sum % 10 == 0
    }

    /// Whole-string validate (digits + checksum).
    pub fn validate(routing: &str) -> bool {
        digits(routing).map(|d| validate_digits(&d)).unwrap_or(false)
    }

    /// First two digits → Federal Reserve district (with thrift/
    /// electronic offsets). `None` for an unrecognized prefix.
    pub fn frb(routing: &str) -> Option<u32> {
        let digits: String = routing.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() != 9 {
            return None;
        }
        let first2: u32 = digits[..2].parse().ok()?;
        match first2 {
            0 => Some(0),
            1..=12 => Some(first2),
            21..=32 => Some(first2 - 20),
            61..=72 => Some(first2 - 60),
            80 => Some(0),
            _ => None,
        }
    }

    /// District number → region name.
    pub fn fed_region(district: u32) -> &'static str {
        match district {
            0 => "U.S. Treasury / federal government",
            1 => "Boston",
            2 => "New York",
            3 => "Philadelphia",
            4 => "Cleveland",
            5 => "Richmond",
            6 => "Atlanta",
            7 => "Chicago",
            8 => "St. Louis",
            9 => "Minneapolis",
            10 => "Kansas City",
            11 => "Dallas",
            12 => "San Francisco",
            _ => "unknown",
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "aba";
    version = env!("CARGO_PKG_VERSION");

    scalar aba_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "aba_validate")?;
        Ok(NeutralValue::Boolean(logic::validate(&raw)))
    };

    scalar aba_frb_district(text) -> int64 [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "aba_frb_district")?;
        Ok(match logic::frb(&raw) {
            Some(d) => NeutralValue::Int64(d as i64),
            None => NeutralValue::Null,
        })
    };

    scalar aba_fed_region(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "aba_fed_region")?;
        Ok(match logic::frb(&raw) {
            Some(d) => NeutralValue::Text(String::from(logic::fed_region(d))),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn declares_the_reconciled_superset() {
        let names: std::vec::Vec<_> = Core::DECLS.iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["aba_validate", "aba_frb_district", "aba_fed_region"]);
    }

    #[test]
    fn validate_matches_baseline() {
        assert_eq!(
            Core::dispatch(idx("aba_validate"), &[t("021000021")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("aba_validate"), &[t("021000020")]).unwrap(),
            NeutralValue::Boolean(false)
        );
        assert_eq!(
            Core::dispatch(idx("aba_validate"), &[t("not digits")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }

    #[test]
    fn frb_and_region() {
        assert_eq!(
            Core::dispatch(idx("aba_frb_district"), &[t("021000021")]).unwrap(),
            NeutralValue::Int64(2)
        );
        assert_eq!(
            Core::dispatch(idx("aba_fed_region"), &[t("322271627")]).unwrap(),
            NeutralValue::Text(String::from("San Francisco"))
        );
        // Unrecognized prefix -> NULL (Option->Null).
        assert_eq!(
            Core::dispatch(idx("aba_frb_district"), &[t("999999999")]).unwrap(),
            NeutralValue::Null
        );
    }
}
