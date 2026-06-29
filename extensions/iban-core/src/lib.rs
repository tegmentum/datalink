//! Neutral core for the `iban` extension — IBAN (ISO 13616) validation + field
//! extraction — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `iban_validate(text) -> boolean`  (ISO 7064 mod-97 == 1)
//!   * `iban_country(text) -> text`      (2-letter country prefix; NULL if absent)
//!   * `iban_bban(text) -> text`         (Basic Bank Account Number after pos 4)
//!
//! Whitespace and hyphens are ignored. A NULL / empty input validates as false
//! and yields NULL for country/bban (the pre-pullup arg_text coerced NULL to "").

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;

    pub fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect::<String>()
            .to_ascii_uppercase()
    }

    pub fn validate(s: &str) -> bool {
        let n = normalize(s);
        let b = n.as_bytes();
        if n.len() < 15 || n.len() > 34 {
            return false;
        }
        if !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic()
            || !b[2].is_ascii_digit() || !b[3].is_ascii_digit()
        {
            return false;
        }
        let mut rem: u32 = 0;
        let mut feed = |c: char| -> bool {
            if c.is_ascii_digit() {
                rem = (rem * 10 + (c as u32 - '0' as u32)) % 97;
                true
            } else if c.is_ascii_alphabetic() {
                let v = c as u32 - 'A' as u32 + 10;
                rem = (rem * 10 + v / 10) % 97;
                rem = (rem * 10 + v % 10) % 97;
                true
            } else {
                false
            }
        };
        for c in n[4..].chars().chain(n[..4].chars()) {
            if !feed(c) {
                return false;
            }
        }
        rem == 1
    }

    pub fn country(s: &str) -> Option<String> {
        let n = normalize(s);
        let b = n.as_bytes();
        if n.len() >= 2 && b[0].is_ascii_alphabetic() && b[1].is_ascii_alphabetic() {
            Some(n[..2].into())
        } else {
            None
        }
    }

    pub fn bban(s: &str) -> Option<String> {
        let n = normalize(s);
        if n.len() > 4 {
            Some(n[4..].into())
        } else {
            None
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "iban";
    version = env!("CARGO_PKG_VERSION");

    scalar iban_validate(text) -> boolean [called, deterministic] = |args| {
        let raw = args.arg_text(0, "iban_validate")?;
        Ok(NeutralValue::Boolean(logic::validate(&raw)))
    };
    scalar iban_country(text) -> text [called, deterministic] = |args| {
        let raw = args.arg_text(0, "iban_country")?;
        Ok(match logic::country(&raw) {
            Some(c) => NeutralValue::Text(c),
            None => NeutralValue::Null,
        })
    };
    scalar iban_bban(text) -> text [called, deterministic] = |args| {
        let raw = args.arg_text(0, "iban_bban")?;
        Ok(match logic::bban(&raw) {
            Some(b) => NeutralValue::Text(b),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("iban_validate"), &[t("GB82 WEST 1234 5698 7654 32")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("iban_validate"), &[t("DE89370400440532013000")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("iban_validate"), &[t("GB82 WEST 1234 5698 7654 33")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("iban_validate"), &[t("not an iban")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("iban_country"), &[t("GB82WEST12345698765432")]).unwrap(), t("GB"));
        assert_eq!(Core::dispatch(idx("iban_bban"), &[t("GB82WEST12345698765432")]).unwrap(), t("WEST12345698765432"));
    }
}
