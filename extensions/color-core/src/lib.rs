//! Neutral core for the `color` extension — WCAG colour math (relative
//! luminance + contrast ratio), parsing via `csscolorparser` — written ONCE.
//! The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `color_luminance(css)  -> float64`  WCAG relative luminance (0..1)
//!   * `color_contrast(a, b)  -> float64`  WCAG contrast ratio (1..21)
//!
//! Unparseable input -> `NULL`, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): the luminance math uses `f64::powf` (a `std`
//! intrinsic); `extern crate alloc` keeps the `declare!`-generated `::alloc`
//! paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// WCAG 2.x relative luminance from a CSS colour string (DB-agnostic).
pub fn luminance(css: &str) -> Option<f64> {
    let c = csscolorparser::parse(css).ok()?;
    let lin = |v: f32| {
        let v = v as f64;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b))
}

datalink_extcore::declare! {
    core = Core;
    extension = "color";
    version = env!("CARGO_PKG_VERSION");

    scalar color_luminance(text) -> float64 [propagate, deterministic] = |args| {
        let css = args.arg_text(0, "color_luminance")?;
        Ok(match luminance(&css) {
            Some(l) => NeutralValue::Float64(l),
            None => NeutralValue::Null,
        })
    };

    scalar color_contrast(text, text) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "color_contrast")?;
        let b = args.arg_text(1, "color_contrast")?;
        Ok(match (luminance(&a), luminance(&b)) {
            (Some(a), Some(b)) => {
                let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
                NeutralValue::Float64((hi + 0.05) / (lo + 0.05))
            }
            _ => NeutralValue::Null,
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
    fn as_f64(v: NeutralValue) -> f64 {
        match v {
            NeutralValue::Float64(x) => x,
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert!((as_f64(Core::dispatch(idx("color_luminance"), &[t("white")]).unwrap()) - 1.0).abs() < 1e-4);
        assert!(as_f64(Core::dispatch(idx("color_luminance"), &[t("black")]).unwrap()).abs() < 1e-4);
        assert!((as_f64(Core::dispatch(idx("color_contrast"), &[t("white"), t("black")]).unwrap()) - 21.0).abs() < 0.1);
        assert!((as_f64(Core::dispatch(idx("color_contrast"), &[t("#777"), t("#fff")]).unwrap()) - 4.48).abs() < 0.01);
        assert_eq!(
            Core::dispatch(idx("color_luminance"), &[t("not-a-color")]).unwrap(),
            NeutralValue::Null
        );
    }
}
