//! Neutral core for the `emoji` extension — emoji lookups (via `emojis`) —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `emoji_name(emoji)      -> text`  the emoji's CLDR name
//!   * `emoji_shortcode(emoji) -> text`  the `:shortcode:` (no colons)
//!   * `emoji_char(shortcode)  -> text`  the emoji for a shortcode
//!
//! Unknown input -> `NULL`, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): `emojis` is a std crate; `extern crate alloc`
//! keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "emoji";
    version = env!("CARGO_PKG_VERSION");

    scalar emoji_name(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "emoji_name")?;
        Ok(match emojis::get(s.trim()) {
            Some(e) => NeutralValue::Text(e.name().to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar emoji_shortcode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "emoji_shortcode")?;
        Ok(match emojis::get(s.trim()).and_then(|e| e.shortcode()) {
            Some(c) => NeutralValue::Text(c.to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar emoji_char(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "emoji_char")?;
        Ok(match emojis::get_by_shortcode(s.trim().trim_matches(':')) {
            Some(e) => NeutralValue::Text(e.as_str().to_string()),
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
            Core::dispatch(idx("emoji_name"), &[t("\u{1F680}")]).unwrap(),
            NeutralValue::Text(String::from("rocket"))
        );
        assert_eq!(
            Core::dispatch(idx("emoji_shortcode"), &[t("\u{1F600}")]).unwrap(),
            NeutralValue::Text(String::from("grinning"))
        );
        assert_eq!(
            Core::dispatch(idx("emoji_char"), &[t("rocket")]).unwrap(),
            NeutralValue::Text(String::from("\u{1F680}"))
        );
        assert_eq!(
            Core::dispatch(idx("emoji_char"), &[t(":tada:")]).unwrap(),
            NeutralValue::Text(String::from("\u{1F389}"))
        );
        assert_eq!(
            Core::dispatch(idx("emoji_name"), &[t("notanemoji")]).unwrap(),
            NeutralValue::Null
        );
    }
}
