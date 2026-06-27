//! Neutral core for the `url` extension — URL component extraction via
//! the `url` crate — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `url_scheme / url_host / url_path / url_query(text) -> text`
//!   * `url_port(text) -> int64`  (scheme-default-aware)
//!
//! NULL / unparseable -> NULL (and individually NULL when the component
//! is absent). The surface is identical in both ports (zero drift).

extern crate alloc;

use datalink_extcore::NeutralValue;
use url::Url;

datalink_extcore::declare! {
    core = Core;
    extension = "url";
    version = env!("CARGO_PKG_VERSION");

    scalar url_scheme(text) -> text [propagate, deterministic] = |args| {
        Ok(match Url::parse(&args.arg_text(0, "url_scheme")?) {
            Ok(u) => NeutralValue::Text(alloc::string::String::from(u.scheme())),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar url_host(text) -> text [propagate, deterministic] = |args| {
        Ok(match Url::parse(&args.arg_text(0, "url_host")?).ok().and_then(|u| u.host_str().map(alloc::string::String::from)) {
            Some(h) => NeutralValue::Text(h),
            None => NeutralValue::Null,
        })
    };
    scalar url_port(text) -> int64 [propagate, deterministic] = |args| {
        Ok(match Url::parse(&args.arg_text(0, "url_port")?).ok().and_then(|u| u.port_or_known_default()) {
            Some(p) => NeutralValue::Int64(p as i64),
            None => NeutralValue::Null,
        })
    };
    scalar url_path(text) -> text [propagate, deterministic] = |args| {
        Ok(match Url::parse(&args.arg_text(0, "url_path")?) {
            Ok(u) => NeutralValue::Text(alloc::string::String::from(u.path())),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar url_query(text) -> text [propagate, deterministic] = |args| {
        Ok(match Url::parse(&args.arg_text(0, "url_query")?).ok().and_then(|u| u.query().map(alloc::string::String::from)) {
            Some(q) => NeutralValue::Text(q),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(alloc::string::String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("url_scheme"), &[t("https://user@ex.com:8443/p/q?a=1#f")]).unwrap(), t("https"));
        assert_eq!(Core::dispatch(idx("url_host"), &[t("https://ex.com:8443/p?a=1")]).unwrap(), t("ex.com"));
        assert_eq!(Core::dispatch(idx("url_port"), &[t("https://ex.com/p")]).unwrap(), NeutralValue::Int64(443));
        assert_eq!(Core::dispatch(idx("url_path"), &[t("https://ex.com/a/b/c")]).unwrap(), t("/a/b/c"));
        assert_eq!(Core::dispatch(idx("url_query"), &[t("https://ex.com/p?a=1&b=2")]).unwrap(), t("a=1&b=2"));
        assert_eq!(Core::dispatch(idx("url_host"), &[t("not a url")]).unwrap(), NeutralValue::Null);
    }
}
