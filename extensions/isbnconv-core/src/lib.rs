//! Neutral core for the `isbnconv` extension — ISBN-10 <-> ISBN-13 conversion —
//! written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `isbn10_to_13(text) -> text`  (978 + first 9 digits + new check)
//!   * `isbn13_to_10(text) -> text`  (978-prefixed only; new check)
//!
//! Invalid length / non-978 -> NULL.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;

    pub fn clean(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }

    pub fn isbn13_check(body12: &str) -> Option<char> {
        if body12.len() != 12 { return None; }
        let mut sum = 0i32;
        for (i, c) in body12.chars().enumerate() {
            let d = c.to_digit(10)? as i32;
            sum += if i % 2 == 0 { d } else { 3 * d };
        }
        Some((b'0' + ((10 - sum % 10) % 10) as u8) as char)
    }

    pub fn isbn10_check(body9: &str) -> Option<char> {
        if body9.len() != 9 { return None; }
        let mut sum = 0i32;
        for (i, c) in body9.chars().enumerate() {
            sum += (10 - i as i32) * c.to_digit(10)? as i32;
        }
        let cd = (11 - sum % 11) % 11;
        Some(if cd == 10 { 'X' } else { (b'0' + cd as u8) as char })
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "isbnconv";
    version = env!("CARGO_PKG_VERSION");

    scalar isbn10_to_13(text) -> text [propagate, deterministic] = |args| {
        let raw: String = logic::clean(&args.arg_text(0, "isbn10_to_13")?);
        if raw.len() != 10 { return Ok(NeutralValue::Null); }
        let body = format!("978{}", &raw[..9]);
        Ok(match logic::isbn13_check(&body) {
            Some(cd) => NeutralValue::Text(format!("{}{}", body, cd)),
            None => NeutralValue::Null,
        })
    };
    scalar isbn13_to_10(text) -> text [propagate, deterministic] = |args| {
        let raw: String = logic::clean(&args.arg_text(0, "isbn13_to_10")?);
        if raw.len() != 13 || !raw.starts_with("978") { return Ok(NeutralValue::Null); }
        let body = &raw[3..12];
        Ok(match logic::isbn10_check(body) {
            Some(cd) => NeutralValue::Text(format!("{}{}", body, cd)),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("isbn10_to_13"), &[t("0306406152")]).unwrap(), t("9780306406157"));
        assert_eq!(Core::dispatch(idx("isbn13_to_10"), &[t("9780306406157")]).unwrap(), t("0306406152"));
        assert_eq!(Core::dispatch(idx("isbn13_to_10"), &[t("9790306406157")]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("isbn10_to_13"), &[t("123")]).unwrap(), NeutralValue::Null);
    }
}
