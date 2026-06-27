//! Neutral core for the `creditcard` extension — PAN validation,
//! network detection, and masking helpers — written ONCE. The per-DB
//! shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Reconciled drift (union)
//!
//! ducklink shipped `cc_validate` + `cc_network` (capitalized brand
//! names); sqlink shipped `cc_validate` + `cc_type` (lowercase brand) +
//! `cc_mask` / `cc_last4` / `cc_bin` / `cc_normalize`. Only `cc_validate`
//! shares a name; it is kept on ducklink's length(12..=19)+Luhn rule for
//! byte-parity (sqlink's brand+Luhn variant is documented residual
//! drift, reconciled with the deferred sqlink shim). Every other name is
//! distinct, so this core exposes BOTH families and each database gains
//! the other's helpers.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// ducklink's strict digit collector (whitespace/hyphens ignored;
    /// `None` on any other non-digit char or empty).
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

    /// sqlink's lenient digit string (strip ALL non-digits).
    pub fn digits_only(s: &str) -> String {
        s.chars().filter(|c| c.is_ascii_digit()).collect()
    }

    pub fn luhn_ok(num: &[u32]) -> bool {
        let mut sum = 0u32;
        let mut alt = false;
        for &d in num.iter().rev() {
            let v = if alt {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            };
            sum += v;
            alt = !alt;
        }
        sum % 10 == 0
    }

    /// ducklink `cc_validate`: plausible length + Luhn.
    pub fn validate(num: &[u32]) -> bool {
        (12..=19).contains(&num.len()) && luhn_ok(num)
    }

    fn prefix(num: &[u32], k: usize) -> u32 {
        num.iter().take(k).fold(0, |a, &d| a * 10 + d)
    }

    /// ducklink `cc_network`: capitalized brand by IIN prefix.
    pub fn network(num: &[u32]) -> Option<&'static str> {
        if num.len() < 12 {
            return None;
        }
        let len = num.len();
        let p2 = prefix(num, 2);
        let p3 = prefix(num, 3);
        let p4 = prefix(num, 4);
        let p6 = prefix(num, 6);
        if num[0] == 4 {
            Some("Visa")
        } else if (51..=55).contains(&p2) || (2221..=2720).contains(&p4) {
            Some("Mastercard")
        } else if p2 == 34 || p2 == 37 {
            Some("American Express")
        } else if p4 == 6011 || p2 == 65 || (644..=649).contains(&p3) || (622126..=622925).contains(&p6) {
            Some("Discover")
        } else if (3528..=3589).contains(&p4) {
            Some("JCB")
        } else if (300..=305).contains(&p3) || p3 == 309 || p2 == 36 || p2 == 38 || p2 == 39 {
            Some("Diners Club")
        } else if p2 == 62 || p2 == 81 {
            Some("UnionPay")
        } else if (p2 == 50 || (56..=69).contains(&p2)) && len >= 12 {
            Some("Maestro")
        } else {
            None
        }
    }

    /// sqlink `cc_type`: lowercase brand over a digit string.
    pub fn brand(d: &str) -> Option<&'static str> {
        if d.is_empty() {
            return None;
        }
        if (d.starts_with("34") || d.starts_with("37")) && d.len() == 15 {
            return Some("amex");
        }
        if d.starts_with('4') && matches!(d.len(), 13 | 16 | 19) {
            return Some("visa");
        }
        if d.len() == 16 {
            if let Some(p2) = d.get(..2).and_then(|s| s.parse::<u32>().ok()) {
                if (51..=55).contains(&p2) {
                    return Some("mastercard");
                }
            }
            if let Some(p4) = d.get(..4).and_then(|s| s.parse::<u32>().ok()) {
                if (2221..=2720).contains(&p4) {
                    return Some("mastercard");
                }
            }
        }
        if matches!(d.len(), 16 | 17 | 18 | 19) {
            if d.starts_with("6011") || d.starts_with("65") {
                return Some("discover");
            }
            if let Some(p3) = d.get(..3).and_then(|s| s.parse::<u32>().ok()) {
                if (644..=649).contains(&p3) {
                    return Some("discover");
                }
            }
        }
        if matches!(d.len(), 16 | 17 | 18 | 19) {
            if let Some(p4) = d.get(..4).and_then(|s| s.parse::<u32>().ok()) {
                if (3528..=3589).contains(&p4) {
                    return Some("jcb");
                }
            }
        }
        if d.len() == 14 {
            if d.starts_with("36") || d.starts_with("38") || d.starts_with("39") {
                return Some("diners");
            }
            if let Some(p3) = d.get(..3).and_then(|s| s.parse::<u32>().ok()) {
                if (300..=305).contains(&p3) {
                    return Some("diners");
                }
            }
        }
        if d.starts_with("62") && matches!(d.len(), 16 | 17 | 18 | 19) {
            return Some("unionpay");
        }
        if matches!(d.len(), 12..=19)
            && (d.starts_with("50") || d.starts_with("56") || d.starts_with("57")
                || d.starts_with("58") || d.starts_with("67"))
        {
            return Some("maestro");
        }
        None
    }

    /// sqlink `cc_mask`: all but the last 4 digits as 'X'.
    pub fn mask(d: &str) -> String {
        if d.len() <= 4 {
            return String::from(d);
        }
        let n = d.len() - 4;
        let mut out = String::with_capacity(d.len());
        for _ in 0..n {
            out.push('X');
        }
        out.push_str(&d[n..]);
        out
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "creditcard";
    version = env!("CARGO_PKG_VERSION");

    // ducklink family — byte-parity preserved.
    scalar cc_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "cc_validate")?;
        Ok(NeutralValue::Boolean(logic::digits(&raw).map(|d| logic::validate(&d)).unwrap_or(false)))
    };
    scalar cc_network(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "cc_network")?;
        Ok(match logic::digits(&raw).and_then(|d| logic::network(&d)) {
            Some(n) => NeutralValue::Text(alloc::string::String::from(n)),
            None => NeutralValue::Null,
        })
    };

    // sqlink family (digit-string based) — the gained superset.
    scalar cc_type(text) -> text [propagate, deterministic] = |args| {
        let d = logic::digits_only(&args.arg_text(0, "cc_type")?);
        Ok(match logic::brand(&d) {
            Some(t) => NeutralValue::Text(alloc::string::String::from(t)),
            None => NeutralValue::Null,
        })
    };
    scalar cc_mask(text) -> text [propagate, deterministic] = |args| {
        let d = logic::digits_only(&args.arg_text(0, "cc_mask")?);
        Ok(if d.is_empty() { NeutralValue::Null } else { NeutralValue::Text(logic::mask(&d)) })
    };
    scalar cc_last4(text) -> text [propagate, deterministic] = |args| {
        let d = logic::digits_only(&args.arg_text(0, "cc_last4")?);
        Ok(if d.len() >= 4 {
            NeutralValue::Text(alloc::string::String::from(&d[d.len() - 4..]))
        } else {
            NeutralValue::Null
        })
    };
    scalar cc_bin(text) -> text [propagate, deterministic] = |args| {
        let d = logic::digits_only(&args.arg_text(0, "cc_bin")?);
        Ok(if d.len() >= 6 {
            NeutralValue::Text(alloc::string::String::from(&d[..6]))
        } else {
            NeutralValue::Null
        })
    };
    scalar cc_normalize(text) -> text [propagate, deterministic] = |args| {
        let d = logic::digits_only(&args.arg_text(0, "cc_normalize")?);
        Ok(if d.is_empty() { NeutralValue::Null } else { NeutralValue::Text(d) })
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
    fn ducklink_family() {
        assert_eq!(Core::dispatch(idx("cc_validate"), &[t("4111 1111 1111 1111")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("cc_validate"), &[t("4111 1111 1111 1112")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("cc_network"), &[t("4111111111111111")]).unwrap(), t("Visa"));
        assert_eq!(Core::dispatch(idx("cc_network"), &[t("5105105105105100")]).unwrap(), t("Mastercard"));
        assert_eq!(Core::dispatch(idx("cc_network"), &[t("340000000000009")]).unwrap(), t("American Express"));
        assert_eq!(Core::dispatch(idx("cc_network"), &[t("6011000990139424")]).unwrap(), t("Discover"));
    }

    #[test]
    fn sqlink_family() {
        // ground-truthed from sqlink smoke.expected
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("4111111111111111")]).unwrap(), t("visa"));
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("5555 5555 5555 4444")]).unwrap(), t("mastercard"));
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("378282246310005")]).unwrap(), t("amex"));
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("6011111111111117")]).unwrap(), t("discover"));
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("3530111333300000")]).unwrap(), t("jcb"));
        assert_eq!(Core::dispatch(idx("cc_type"), &[t("not a card")]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("cc_mask"), &[t("4111-1111-1111-1111")]).unwrap(), t("XXXXXXXXXXXX1111"));
        assert_eq!(Core::dispatch(idx("cc_last4"), &[t("4111-1111-1111-1111")]).unwrap(), t("1111"));
        assert_eq!(Core::dispatch(idx("cc_bin"), &[t("4111-1111-1111-1111")]).unwrap(), t("411111"));
        assert_eq!(Core::dispatch(idx("cc_normalize"), &[t("4111 1111 1111 1111")]).unwrap(), t("4111111111111111"));
    }
}
