//! Neutral core for the `currency` extension — ISO 4217 currency lookups
//! (via `iso_currency`), keyed by alphabetic code — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare) table.
//!
//! # Functions
//!
//!   * `currency_name(code)     -> text`
//!   * `currency_symbol(code)   -> text`
//!   * `currency_numeric(code)  -> int64`
//!   * `currency_exponent(code) -> int64`  (minor-unit digits; `NULL` if none)
//!
//! Unknown code -> `NULL`, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): `iso_currency` is a std crate; `extern crate alloc`
//! keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;
use iso_currency::Currency;

datalink_extcore::declare! {
    core = Core;
    extension = "currency";
    version = env!("CARGO_PKG_VERSION");

    scalar currency_name(text) -> text [propagate, deterministic] = |args| {
        let code = args.arg_text(0, "currency_name")?;
        Ok(match Currency::from_code(&code.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Text(c.name().to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar currency_symbol(text) -> text [propagate, deterministic] = |args| {
        let code = args.arg_text(0, "currency_symbol")?;
        Ok(match Currency::from_code(&code.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Text(c.symbol().to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar currency_numeric(text) -> int64 [propagate, deterministic] = |args| {
        let code = args.arg_text(0, "currency_numeric")?;
        Ok(match Currency::from_code(&code.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Int64(c.numeric() as i64),
            None => NeutralValue::Null,
        })
    };

    scalar currency_exponent(text) -> int64 [propagate, deterministic] = |args| {
        let code = args.arg_text(0, "currency_exponent")?;
        Ok(match Currency::from_code(&code.to_ascii_uppercase()) {
            Some(c) => match c.exponent() {
                Some(e) => NeutralValue::Int64(e as i64),
                None => NeutralValue::Null,
            },
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

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert_eq!(
            Core::dispatch(idx("currency_name"), &[t("USD")]).unwrap(),
            NeutralValue::Text(String::from("United States dollar"))
        );
        assert_eq!(
            Core::dispatch(idx("currency_numeric"), &[t("EUR")]).unwrap(),
            NeutralValue::Int64(978)
        );
        assert_eq!(
            Core::dispatch(idx("currency_exponent"), &[t("JPY")]).unwrap(),
            NeutralValue::Int64(0)
        );
        assert_eq!(
            Core::dispatch(idx("currency_exponent"), &[t("USD")]).unwrap(),
            NeutralValue::Int64(2)
        );
        assert_eq!(
            Core::dispatch(idx("currency_name"), &[t("ZZZ")]).unwrap(),
            NeutralValue::Null
        );
    }
}
