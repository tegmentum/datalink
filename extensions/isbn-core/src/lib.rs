//! Neutral core for the `isbn` extension — ISBN-10 / ISBN-13 validation +
//! normalization — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `isbn_valid(text) -> boolean`  (NULL / invalid -> false)
//!   * `isbn_normalize(text) -> text` (digits-only body, NULL if invalid)

#![no_std]

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;

    /// Strip separators, keeping ASCII digits and an upper/lowercase X (only
    /// valid as the ISBN-10 check digit); X is uppercased.
    pub fn clean(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }

    pub fn valid_isbn10(s: &str) -> bool {
        if s.len() != 10 { return false; }
        let mut sum: i32 = 0;
        for (i, c) in s.chars().enumerate() {
            let v = if i == 9 && c == 'X' { 10 }
                else if let Some(d) = c.to_digit(10) { d as i32 } else { return false };
            sum += (10 - i as i32) * v;
        }
        sum % 11 == 0
    }

    pub fn valid_isbn13(s: &str) -> bool {
        if s.len() != 13 { return false; }
        let mut sum: i32 = 0;
        for (i, c) in s.chars().enumerate() {
            let d = match c.to_digit(10) { Some(d) => d as i32, None => return false };
            sum += if i % 2 == 0 { d } else { 3 * d };
        }
        sum % 10 == 0
    }

    pub fn is_valid(body: &str) -> bool {
        match body.len() { 10 => valid_isbn10(body), 13 => valid_isbn13(body), _ => false }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "isbn";
    version = env!("CARGO_PKG_VERSION");

    scalar isbn_valid(text) -> boolean [called, deterministic] = |args| {
        let raw = args.arg_text(0, "isbn_valid")?;
        Ok(NeutralValue::Boolean(logic::is_valid(&logic::clean(&raw))))
    };
    scalar isbn_normalize(text) -> text [called, deterministic] = |args| {
        let raw = args.arg_text(0, "isbn_normalize")?;
        let body: String = logic::clean(&raw);
        Ok(if logic::is_valid(&body) { NeutralValue::Text(body) } else { NeutralValue::Null })
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
        assert_eq!(Core::dispatch(idx("isbn_valid"), &[t("0-306-40615-2")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("isbn_valid"), &[t("978-0-306-40615-7")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("isbn_valid"), &[t("0-306-40615-3")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("isbn_normalize"), &[t("0 306 40615 2")]).unwrap(), t("0306406152"));
        assert_eq!(Core::dispatch(idx("isbn_normalize"), &[t("not-an-isbn")]).unwrap(), NeutralValue::Null);
    }
}
