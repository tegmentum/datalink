//! Neutral core for the `ean` extension — EAN / UPC barcode helpers —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `ean_validate(text) -> boolean` — valid EAN-8, UPC-A (12), or EAN-13
//!   * `ean_check_digit(text) -> int64` — the check digit for the body digits
//!   * `ean_gs1_prefix(text) -> int64` — the 3-digit GS1 prefix of an EAN-13
//!   * `upca_to_ean13(text) -> text` — a 12-digit UPC-A as a 13-digit EAN
//!
//! # Reconciled drift (additive superset)
//!
//! ducklink shipped `ean_validate` + `ean_check_digit`; sqlink also had
//! `ean_gs1_prefix` + `upca_to_ean13`. This core adopts the 4-function
//! SUPERSET, so ducklink GAINS the two GS1 helpers. The shared
//! `ean_check_digit` keeps ducklink's any-length behaviour (sqlink's
//! EAN-13-only variant is documented residual drift, reconciled with the
//! deferred sqlink shim).

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// EAN algorithm (DB-agnostic), byte-for-byte the ducklink logic.
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Collect digits, ignoring whitespace and hyphens; `None` on any
    /// other non-digit char or an empty result (ducklink's strict form).
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

    /// Check digit for `body` (data digits WITHOUT the trailing check).
    pub fn check_digit(body: &[u32]) -> u32 {
        let mut sum = 0u32;
        for (i, &d) in body.iter().rev().enumerate() {
            sum += if i % 2 == 0 { d * 3 } else { d };
        }
        (10 - (sum % 10)) % 10
    }

    pub fn validate(num: &[u32]) -> bool {
        matches!(num.len(), 8 | 12 | 13) && check_digit(&num[..num.len() - 1]) == num[num.len() - 1]
    }

    /// The 3-digit GS1 prefix of a full 13-digit EAN.
    pub fn gs1_prefix(num: &[u32]) -> Option<u32> {
        if num.len() != 13 {
            return None;
        }
        Some(num[0] * 100 + num[1] * 10 + num[2])
    }

    /// A 12-digit UPC-A as a 13-digit EAN (leading zero).
    pub fn upca_to_ean13(num: &[u32]) -> Option<String> {
        if num.len() != 12 {
            return None;
        }
        let mut out = String::with_capacity(13);
        out.push('0');
        for &d in num {
            out.push(core::char::from_digit(d, 10).unwrap());
        }
        Some(out)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ean";
    version = env!("CARGO_PKG_VERSION");

    scalar ean_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "ean_validate")?;
        Ok(NeutralValue::Boolean(logic::digits(&raw).map(|d| logic::validate(&d)).unwrap_or(false)))
    };
    scalar ean_check_digit(text) -> int64 [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "ean_check_digit")?;
        Ok(match logic::digits(&raw) {
            Some(d) => NeutralValue::Int64(logic::check_digit(&d) as i64),
            None => NeutralValue::Null,
        })
    };
    scalar ean_gs1_prefix(text) -> int64 [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "ean_gs1_prefix")?;
        Ok(match logic::digits(&raw).and_then(|d| logic::gs1_prefix(&d)) {
            Some(p) => NeutralValue::Int64(p as i64),
            None => NeutralValue::Null,
        })
    };
    scalar upca_to_ean13(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "upca_to_ean13")?;
        Ok(match logic::digits(&raw).and_then(|d| logic::upca_to_ean13(&d)) {
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

    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn baseline_and_superset() {
        // ducklink baseline
        assert_eq!(Core::dispatch(idx("ean_validate"), &[t("4006381333931")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("ean_validate"), &[t("96385074")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("ean_validate"), &[t("4006381333930")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("ean_check_digit"), &[t("400638133393")]).unwrap(), NeutralValue::Int64(1));
        assert_eq!(Core::dispatch(idx("ean_check_digit"), &[t("9638507")]).unwrap(), NeutralValue::Int64(4));
        // superset (ground-truthed from sqlink smoke.expected)
        assert_eq!(Core::dispatch(idx("ean_gs1_prefix"), &[t("4006381333931")]).unwrap(), NeutralValue::Int64(400));
        assert_eq!(Core::dispatch(idx("ean_gs1_prefix"), &[t("5901234123457")]).unwrap(), NeutralValue::Int64(590));
        assert_eq!(Core::dispatch(idx("upca_to_ean13"), &[t("036000291452")]).unwrap(), t("0036000291452"));
    }
}
