//! Neutral core for the `csscolor` extension — CSS colour parsing +
//! conversion (via `csscolorparser`) — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `css_to_hex(color) -> text`     any CSS colour -> `#rrggbb[aa]`
//!   * `css_to_rgb(color) -> text`     -> `rgb(r g b)` / `rgba(...)`
//!   * `css_valid(color)  -> boolean`  true if it parses
//!
//! Unparseable input -> `NULL` (`css_to_*`) or `false` (`css_valid`),
//! byte-for-byte the pre-pullup behaviour. `css_valid` declares `called`
//! NULL-handling so a NULL argument yields `false` (not `NULL`), matching the
//! baseline; `css_to_*` declare `propagate` (NULL -> NULL).
//!
//! `std` (not `no_std`): `csscolorparser` is a std crate; `extern crate alloc`
//! keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "csscolor";
    version = env!("CARGO_PKG_VERSION");

    scalar css_to_hex(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "css_to_hex")?;
        Ok(match csscolorparser::parse(&s) {
            Ok(c) => NeutralValue::Text(c.to_hex_string()),
            Err(_) => NeutralValue::Null,
        })
    };

    scalar css_to_rgb(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "css_to_rgb")?;
        Ok(match csscolorparser::parse(&s) {
            Ok(c) => NeutralValue::Text(c.to_css_rgb()),
            Err(_) => NeutralValue::Null,
        })
    };

    scalar css_valid(text) -> boolean [called, deterministic] = |args| {
        // `called`: a NULL argument coerces to "" (ArgExt::arg_text), which
        // does not parse -> false, matching the baseline's None -> false.
        let s = args.arg_text(0, "css_valid")?;
        Ok(NeutralValue::Boolean(csscolorparser::parse(&s).is_ok()))
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
            Core::dispatch(idx("css_to_hex"), &[t("red")]).unwrap(),
            NeutralValue::Text(String::from("#ff0000"))
        );
        assert_eq!(
            Core::dispatch(idx("css_to_hex"), &[t("rgb(0,128,255)")]).unwrap(),
            NeutralValue::Text(String::from("#0080ff"))
        );
        assert_eq!(
            Core::dispatch(idx("css_to_rgb"), &[t("#ff8800")]).unwrap(),
            NeutralValue::Text(String::from("rgb(255 136 0)"))
        );
        assert_eq!(
            Core::dispatch(idx("css_valid"), &[t("rebeccapurple")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("css_valid"), &[t("not-a-color")]).unwrap(),
            NeutralValue::Boolean(false)
        );
        assert_eq!(
            Core::dispatch(idx("css_to_hex"), &[t("not-a-color")]).unwrap(),
            NeutralValue::Null
        );
        // NULL -> false for css_valid (called null-handling).
        assert_eq!(
            Core::dispatch(idx("css_valid"), &[NeutralValue::Null]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }
}
