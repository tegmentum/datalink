//! Neutral core for the `escape` extension — HTML + URL percent encode /
//! decode — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `html_escape(text) -> text`   — HTML-entity encode.
//!   * `html_unescape(text) -> text` — HTML-entity decode.
//!   * `url_encode(text) -> text`    — percent-encode (NON_ALPHANUMERIC).
//!   * `url_decode(text) -> text`    — percent-decode (lossy UTF-8).
//!
//! NULL -> NULL (propagate). The surface is identical in both ports.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};

pub fn html_escape(s: &str) -> String {
    html_escape::encode_text(s).into_owned()
}

pub fn html_unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

pub fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

pub fn url_decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

datalink_extcore::declare! {
    core = Core;
    extension = "escape";
    version = env!("CARGO_PKG_VERSION");

    scalar html_escape(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(html_escape(&args.arg_text(0, "html_escape")?)))
    };

    scalar html_unescape(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(html_unescape(&args.arg_text(0, "html_unescape")?)))
    };

    scalar url_encode(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(url_encode(&args.arg_text(0, "url_encode")?)))
    };

    scalar url_decode(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(url_decode(&args.arg_text(0, "url_decode")?)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }

    #[test]
    fn roundtrips() {
        assert_eq!(Core::dispatch(idx("html_escape"), &[t("<a>&")]).unwrap(), t("&lt;a&gt;&amp;"));
        assert_eq!(Core::dispatch(idx("html_unescape"), &[t("&lt;a&gt;&amp;")]).unwrap(), t("<a>&"));
        assert_eq!(Core::dispatch(idx("url_encode"), &[t("a b/c")]).unwrap(), t("a%20b%2Fc"));
        assert_eq!(Core::dispatch(idx("url_decode"), &[t("a%20b%2Fc")]).unwrap(), t("a b/c"));
    }
}
