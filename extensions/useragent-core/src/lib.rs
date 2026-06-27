//! Neutral core for the `useragent` extension — User-Agent string
//! parsing via `woothee` — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ua_browser / ua_browser_version / ua_os / ua_category(ua) -> text`
//!   * `ua_is_bot(ua) -> boolean`  (true if the UA is a crawler)
//!
//! NULL -> NULL; an unparseable UA yields woothee's `UNKNOWN`. The
//! surface is identical in both ports (zero drift).

extern crate alloc;

use alloc::string::{String, ToString};
use datalink_extcore::NeutralValue;
use woothee::parser::{Parser, WootheeResult};
use woothee::woothee::VALUE_UNKNOWN;

fn parse_field(ua: &str, f: for<'a> fn(&'a WootheeResult<'a>) -> &'a str) -> String {
    match Parser::new().parse(ua) {
        Some(r) => f(&r).to_string(),
        None => VALUE_UNKNOWN.to_string(),
    }
}

fn is_bot(ua: &str) -> bool {
    matches!(Parser::new().parse(ua), Some(r) if r.category == "crawler")
}

datalink_extcore::declare! {
    core = Core;
    extension = "useragent";
    version = env!("CARGO_PKG_VERSION");

    scalar ua_browser(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(parse_field(&args.arg_text(0, "ua_browser")?, |r| r.name)))
    };
    scalar ua_browser_version(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(parse_field(&args.arg_text(0, "ua_browser_version")?, |r| r.version)))
    };
    scalar ua_os(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(parse_field(&args.arg_text(0, "ua_os")?, |r| r.os)))
    };
    scalar ua_category(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(parse_field(&args.arg_text(0, "ua_category")?, |r| r.category)))
    };
    scalar ua_is_bot(text) -> boolean [propagate, deterministic] = |args| {
        Ok(NeutralValue::Boolean(is_bot(&args.arg_text(0, "ua_is_bot")?)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
    const BOT: &str = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("ua_browser"), &[t(CHROME)]).unwrap(), t("Chrome"));
        assert_eq!(Core::dispatch(idx("ua_os"), &[t(CHROME)]).unwrap(), t("Windows 10"));
        assert_eq!(Core::dispatch(idx("ua_category"), &[t(CHROME)]).unwrap(), t("pc"));
        assert_eq!(Core::dispatch(idx("ua_is_bot"), &[t(CHROME)]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("ua_category"), &[t(BOT)]).unwrap(), t("crawler"));
        assert_eq!(Core::dispatch(idx("ua_is_bot"), &[t(BOT)]).unwrap(), NeutralValue::Boolean(true));
    }
}
