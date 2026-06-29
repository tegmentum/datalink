//! Neutral core for the `colorconv` extension — hand-rolled HSL / HSV colour
//! conversions — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `hex_to_hsl(hex) -> text`  -> "h,s,l" (h 0-360, s/l 0-100)
//!   * `hex_to_hsv(hex) -> text`  -> "h,s,v"
//!   * `hsl_to_hex(h, s, l) -> text` -> "#rrggbb"
//!
//! A bad hex string yields `NULL`, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): the colour math uses `f64` rounding/abs intrinsics
//! that live in `std`. `extern crate alloc` keeps the `declare!`-generated
//! `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    pub fn parse_hex(s: &str) -> Option<(f64, f64, f64)> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&h[0..2], 16).ok()?;
        let g = u8::from_str_radix(&h[2..4], 16).ok()?;
        let b = u8::from_str_radix(&h[4..6], 16).ok()?;
        Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
    }

    pub fn hue(r: f64, g: f64, b: f64, max: f64, d: f64) -> f64 {
        if d == 0.0 {
            return 0.0;
        }
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h * 60.0
    }

    pub fn to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let l = (max + min) / 2.0;
        let s = if d == 0.0 {
            0.0
        } else {
            d / (1.0 - (2.0 * l - 1.0).abs())
        };
        (hue(r, g, b, max, d), s * 100.0, l * 100.0)
    }

    pub fn to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let s = if max == 0.0 { 0.0 } else { d / max };
        (hue(r, g, b, max, d), s * 100.0, max * 100.0)
    }

    pub fn hue2rgb(p: f64, q: f64, mut t: f64) -> f64 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    }

    pub fn hsl_to_hex(h: f64, s: f64, l: f64) -> String {
        let (h, s, l) = (
            h / 360.0,
            (s / 100.0).clamp(0.0, 1.0),
            (l / 100.0).clamp(0.0, 1.0),
        );
        let (r, g, b) = if s == 0.0 {
            (l, l, l)
        } else {
            let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
            let p = 2.0 * l - q;
            (
                hue2rgb(p, q, h + 1.0 / 3.0),
                hue2rgb(p, q, h),
                hue2rgb(p, q, h - 1.0 / 3.0),
            )
        };
        alloc::format!(
            "#{:02x}{:02x}{:02x}",
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8
        )
    }

    pub fn fmt3(a: f64, b: f64, c: f64) -> String {
        alloc::format!("{},{},{}", a.round() as i64, b.round() as i64, c.round() as i64)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "colorconv";
    version = env!("CARGO_PKG_VERSION");

    scalar hex_to_hsl(text) -> text [propagate, deterministic] = |args| {
        let hex = args.arg_text(0, "hex_to_hsl")?;
        match logic::parse_hex(&hex) {
            Some((r, g, b)) => {
                let (h, s, l) = logic::to_hsl(r, g, b);
                Ok(NeutralValue::Text(logic::fmt3(h, s, l)))
            }
            None => Ok(NeutralValue::Null),
        }
    };

    scalar hex_to_hsv(text) -> text [propagate, deterministic] = |args| {
        let hex = args.arg_text(0, "hex_to_hsv")?;
        match logic::parse_hex(&hex) {
            Some((r, g, b)) => {
                let (h, s, v) = logic::to_hsv(r, g, b);
                Ok(NeutralValue::Text(logic::fmt3(h, s, v)))
            }
            None => Ok(NeutralValue::Null),
        }
    };

    scalar hsl_to_hex(float64, float64, float64) -> text [propagate, deterministic] = |args| {
        let h = args.arg_float(0, "hsl_to_hex")?;
        let s = args.arg_float(1, "hsl_to_hex")?;
        let l = args.arg_float(2, "hsl_to_hex")?;
        Ok(NeutralValue::Text(logic::hsl_to_hex(h, s, l)))
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
    fn f(v: f64) -> NeutralValue {
        NeutralValue::Float64(v)
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert_eq!(
            Core::dispatch(idx("hex_to_hsl"), &[t("#ff0000")]).unwrap(),
            NeutralValue::Text(String::from("0,100,50"))
        );
        assert_eq!(
            Core::dispatch(idx("hex_to_hsv"), &[t("#ff0000")]).unwrap(),
            NeutralValue::Text(String::from("0,100,100"))
        );
        assert_eq!(
            Core::dispatch(idx("hex_to_hsl"), &[t("#808080")]).unwrap(),
            NeutralValue::Text(String::from("0,0,50"))
        );
        assert_eq!(
            Core::dispatch(idx("hsl_to_hex"), &[f(0.0), f(100.0), f(50.0)]).unwrap(),
            NeutralValue::Text(String::from("#ff0000"))
        );
        assert_eq!(
            Core::dispatch(idx("hsl_to_hex"), &[f(120.0), f(100.0), f(50.0)]).unwrap(),
            NeutralValue::Text(String::from("#00ff00"))
        );
        assert_eq!(
            Core::dispatch(idx("hex_to_hsl"), &[t("nothex")]).unwrap(),
            NeutralValue::Null
        );
    }
}
