//! Neutral core for the `iso` extension — ISO 3166 country lookups keyed by
//! alpha-2 code (via `rust_iso3166`) — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `iso_country_name(text) -> text`
//!   * `iso_country_alpha3(text) -> text`
//!   * `iso_country_numeric(text) -> int64`
//!
//! Unknown code / NULL -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "iso";
    version = env!("CARGO_PKG_VERSION");

    scalar iso_country_name(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "iso_country_name")?;
        Ok(match rust_iso3166::from_alpha2(&s.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Text(String::from(c.name)),
            None => NeutralValue::Null,
        })
    };
    scalar iso_country_alpha3(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "iso_country_alpha3")?;
        Ok(match rust_iso3166::from_alpha2(&s.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Text(String::from(c.alpha3)),
            None => NeutralValue::Null,
        })
    };
    scalar iso_country_numeric(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "iso_country_numeric")?;
        Ok(match rust_iso3166::from_alpha2(&s.to_ascii_uppercase()) {
            Some(c) => NeutralValue::Int64(c.numeric as i64),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("iso_country_name"), &[t("US")]).unwrap(), t("United States of America"));
        assert_eq!(Core::dispatch(idx("iso_country_alpha3"), &[t("de")]).unwrap(), t("DEU"));
        assert_eq!(Core::dispatch(idx("iso_country_numeric"), &[t("JP")]).unwrap(), NeutralValue::Int64(392));
        assert_eq!(Core::dispatch(idx("iso_country_name"), &[t("ZZ")]).unwrap(), NeutralValue::Null);
    }
}
