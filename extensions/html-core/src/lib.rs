//! Neutral core for the `html` extension — HTML parsing + CSS-selector
//! extraction via `scraper` — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `html_extract(html, css) -> text`      (first matching element)
//!   * `html_extract_all(html, css) -> text`  (JSON array of all matches)
//!   * `html_attr(html, css, attr) -> text`   (attribute of first match)
//!
//! Distinct from `html2text` (which strips a whole document); here
//! extraction is driven by a CSS selector. NULL on NULL input / invalid
//! selector / no match. The surface is identical in both ports (zero
//! drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use scraper::{Html, Selector};

/// Collapse an element's descendant text nodes into a single trimmed string.
fn element_text(el: scraper::ElementRef) -> String {
    let joined: String = el.text().collect::<String>();
    joined.split_whitespace().collect::<alloc::vec::Vec<_>>().join(" ")
}

/// Minimal JSON string escaping for the `html_extract_all` array output.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

datalink_extcore::declare! {
    core = Core;
    extension = "html";
    version = env!("CARGO_PKG_VERSION");

    scalar html_extract(text, text) -> text [propagate, deterministic] = |args| {
        let html = args.arg_text(0, "html_extract")?;
        let css = args.arg_text(1, "html_extract")?;
        let sel = match Selector::parse(&css) { Ok(s) => s, Err(_) => return Ok(NeutralValue::Null) };
        let doc = Html::parse_fragment(&html);
        Ok(match doc.select(&sel).next() {
            Some(el) => NeutralValue::Text(element_text(el)),
            None => NeutralValue::Null,
        })
    };
    scalar html_extract_all(text, text) -> text [propagate, deterministic] = |args| {
        let html = args.arg_text(0, "html_extract_all")?;
        let css = args.arg_text(1, "html_extract_all")?;
        let sel = match Selector::parse(&css) { Ok(s) => s, Err(_) => return Ok(NeutralValue::Null) };
        let doc = Html::parse_fragment(&html);
        let mut json = String::from("[");
        let mut first = true;
        for el in doc.select(&sel) {
            if !first { json.push(','); }
            first = false;
            json.push_str(&json_escape(&element_text(el)));
        }
        json.push(']');
        Ok(NeutralValue::Text(json))
    };
    scalar html_attr(text, text, text) -> text [propagate, deterministic] = |args| {
        let html = args.arg_text(0, "html_attr")?;
        let css = args.arg_text(1, "html_attr")?;
        let attr = args.arg_text(2, "html_attr")?;
        let sel = match Selector::parse(&css) { Ok(s) => s, Err(_) => return Ok(NeutralValue::Null) };
        let doc = Html::parse_fragment(&html);
        Ok(match doc.select(&sel).next().and_then(|el| el.value().attr(&attr).map(String::from)) {
            Some(v) => NeutralValue::Text(v),
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
    const DOC: &str = r#"<ul><li class="x">one</li><li>two</li></ul><a href="/h">link</a>"#;

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("html_extract"), &[t(DOC), t("li.x")]).unwrap(), t("one"));
        assert_eq!(Core::dispatch(idx("html_extract_all"), &[t(DOC), t("li")]).unwrap(), t(r#"["one","two"]"#));
        assert_eq!(Core::dispatch(idx("html_attr"), &[t(DOC), t("a"), t("href")]).unwrap(), t("/h"));
        assert_eq!(Core::dispatch(idx("html_extract"), &[t(DOC), t("p.none")]).unwrap(), NeutralValue::Null);
    }
}
