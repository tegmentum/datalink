//! Neutral core for the `qrcode` extension — QR code generation (SVG
//! output) via the `qrcode` crate — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `qr_svg(text) -> text`  (an SVG document, ~21x21+ modules)
//!
//! Data too large for a QR symbol -> NULL. The surface is identical in
//! both ports (zero drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use qrcode::{render::svg, QrCode};

pub fn qr_svg(s: &str) -> Option<String> {
    match QrCode::new(s.as_bytes()) {
        Ok(code) => Some(
            code.render::<svg::Color>()
                .min_dimensions(200, 200)
                .dark_color(svg::Color("#000000"))
                .light_color(svg::Color("#ffffff"))
                .build(),
        ),
        Err(_) => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "qrcode";
    version = env!("CARGO_PKG_VERSION");

    scalar qr_svg(text) -> text [propagate, deterministic] = |args| {
        Ok(match qr_svg(&args.arg_text(0, "qr_svg")?) {
            Some(s) => NeutralValue::Text(s),
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

    #[test]
    fn matches_baseline() {
        match Core::dispatch(idx("qr_svg"), &[t("hello")]).unwrap() {
            NeutralValue::Text(s) => assert!(s.contains("<svg") && s.len() > 100),
            o => panic!("{o:?}"),
        }
    }
}
