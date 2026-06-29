//! Neutral core for the `bbcode` extension — BBCode -> HTML (the common
//! paired tags [b][i][u][s][code][quote], [url=href]text[/url],
//! [img]src[/img]; unknown tags pass through) — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare) table
//! below.
//!
//!   * `bbcode_to_html(text) -> text`

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::format;
    use alloc::string::{String, ToString};

    /// Replace `[url=HREF]TEXT[/url]` occurrences left-to-right (regex-free).
    fn replace_url(mut s: String) -> String {
        loop {
            let Some(start) = s.find("[url=") else { break };
            let Some(rb_rel) = s[start..].find(']') else { break };
            let href_end = start + rb_rel;
            let Some(close_rel) = s[href_end..].find("[/url]") else { break };
            let text_start = href_end + 1;
            let text_end = href_end + close_rel;
            let href = s[start + 5..href_end].to_string();
            let text = s[text_start..text_end].to_string();
            let after = s[text_end + 6..].to_string();
            let before = s[..start].to_string();
            s = format!("{}<a href=\"{}\">{}</a>{}", before, href, text, after);
        }
        s
    }

    pub fn bbcode(input: &str) -> String {
        let mut s = input.to_string();
        for (bb, html) in [
            ("[b]", "<strong>"),
            ("[/b]", "</strong>"),
            ("[i]", "<em>"),
            ("[/i]", "</em>"),
            ("[u]", "<u>"),
            ("[/u]", "</u>"),
            ("[s]", "<s>"),
            ("[/s]", "</s>"),
            ("[code]", "<code>"),
            ("[/code]", "</code>"),
            ("[quote]", "<blockquote>"),
            ("[/quote]", "</blockquote>"),
            ("[img]", "<img src=\""),
            ("[/img]", "\">"),
        ] {
            s = s.replace(bb, html);
        }
        replace_url(s)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "bbcode";
    version = env!("CARGO_PKG_VERSION");

    scalar bbcode_to_html(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "bbcode_to_html")?;
        Ok(NeutralValue::Text(logic::bbcode(&s)))
    };
}
