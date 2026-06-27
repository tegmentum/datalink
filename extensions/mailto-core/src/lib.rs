//! Neutral core for the `mailto` extension — RFC 6068 `mailto:` URI
//! parsing — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `mailto_to(uri) -> text`         (JSON array of recipients)
//!   * `mailto_field(uri, name) -> text` (percent-decoded header value)
//!   * `mailto_to_json(uri) -> text`    ({to,subject,body,cc,bcc} JSON)
//!
//! Non-mailto / malformed / NULL -> NULL. Never panics. The surface is
//! identical in both ports (zero drift).

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Parsing + JSON encoding (DB-agnostic).
pub mod logic {
    use super::*;
    use percent_encoding::percent_decode_str;

    /// Percent-decode a component. Per RFC 6068, '+' is a literal plus,
    /// NOT a space, so we decode only %XX.
    pub fn pct_decode(s: &str) -> Option<String> {
        percent_decode_str(s).decode_utf8().ok().map(|c| c.into_owned())
    }

    /// A parsed mailto: URI. Addresses + header values are decoded.
    pub struct Mailto {
        pub to: Vec<String>,
        pub headers: BTreeMap<String, String>,
    }

    pub fn parse_mailto(uri: &str) -> Option<Mailto> {
        let rest = uri
            .strip_prefix("mailto:")
            .or_else(|| uri.strip_prefix("MAILTO:"))
            .or_else(|| {
                let lower = uri.get(..7)?.to_ascii_lowercase();
                if lower == "mailto:" {
                    uri.get(7..)
                } else {
                    None
                }
            })?;

        let (to_part, query) = match rest.find('?') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };

        let mut to: Vec<String> = Vec::new();
        if !to_part.is_empty() {
            for addr in to_part.split(',') {
                if addr.is_empty() {
                    continue;
                }
                let d = pct_decode(addr)?;
                if d.is_empty() {
                    return None;
                }
                to.push(d);
            }
        }

        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(q) = query {
            for pair in q.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = match pair.find('=') {
                    Some(i) => (&pair[..i], &pair[i + 1..]),
                    None => return None,
                };
                let name_dec = pct_decode(name)?;
                let value_dec = pct_decode(value)?;
                let key = name_dec.to_ascii_lowercase();
                if key == "to" {
                    for addr in value_dec.split(',') {
                        if !addr.is_empty() {
                            to.push(addr.to_string());
                        }
                    }
                }
                headers.entry(key).or_insert(value_dec);
            }
        }

        Some(Mailto { to, headers })
    }

    pub fn json_escape(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&alloc::format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }

    pub fn json_array(items: &[String]) -> String {
        let mut out = String::new();
        out.push('[');
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_escape(it, &mut out);
        }
        out.push(']');
        out
    }

    pub fn mailto_to_json_str(m: &Mailto) -> String {
        let mut out = String::new();
        out.push('{');
        let mut first = true;
        let comma = |out: &mut String, first: &mut bool| {
            if *first {
                *first = false;
            } else {
                out.push(',');
            }
        };

        if !m.to.is_empty() {
            comma(&mut out, &mut first);
            out.push_str("\"to\":");
            out.push_str(&json_array(&m.to));
        }
        for key in ["subject", "body", "cc", "bcc"] {
            if let Some(v) = m.headers.get(key) {
                comma(&mut out, &mut first);
                json_escape(key, &mut out);
                out.push(':');
                json_escape(v, &mut out);
            }
        }
        out.push('}');
        out
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "mailto";
    version = env!("CARGO_PKG_VERSION");

    scalar mailto_to(text) -> text [propagate, deterministic] = |args| {
        let uri = args.arg_text(0, "mailto_to")?;
        Ok(match logic::parse_mailto(&uri) {
            Some(m) => NeutralValue::Text(logic::json_array(&m.to)),
            None => NeutralValue::Null,
        })
    };
    scalar mailto_field(text, text) -> text [propagate, deterministic] = |args| {
        let uri = args.arg_text(0, "mailto_field")?;
        let name = args.arg_text(1, "mailto_field")?.to_ascii_lowercase();
        Ok(match logic::parse_mailto(&uri) {
            Some(m) => match m.headers.get(name.as_str()) {
                Some(v) => NeutralValue::Text(v.clone()),
                None => NeutralValue::Null,
            },
            None => NeutralValue::Null,
        })
    };
    scalar mailto_to_json(text) -> text [propagate, deterministic] = |args| {
        let uri = args.arg_text(0, "mailto_to_json")?;
        Ok(match logic::parse_mailto(&uri) {
            Some(m) => NeutralValue::Text(logic::mailto_to_json_str(&m)),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    const U: &str = "mailto:alice@example.com?subject=Hello%20World&cc=bob@example.com";

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("mailto_to"), &[t(U)]).unwrap(), t(r#"["alice@example.com"]"#));
        assert_eq!(Core::dispatch(idx("mailto_field"), &[t(U), t("subject")]).unwrap(), t("Hello World"));
        assert_eq!(Core::dispatch(idx("mailto_field"), &[t(U), t("cc")]).unwrap(), t("bob@example.com"));
        assert_eq!(Core::dispatch(idx("mailto_field"), &[t(U), t("bcc")]).unwrap(), NeutralValue::Null);
        assert_eq!(
            Core::dispatch(idx("mailto_to_json"), &[t("mailto:alice@example.com,carol@example.com?subject=Hi&body=Yo&cc=bob@example.com")]).unwrap(),
            t(r#"{"to":["alice@example.com","carol@example.com"],"subject":"Hi","body":"Yo","cc":"bob@example.com"}"#)
        );
        assert_eq!(Core::dispatch(idx("mailto_to"), &[t("https://example.com/not-a-mailto")]).unwrap(), NeutralValue::Null);
    }
}
