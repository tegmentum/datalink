//! Neutral core for the `textplot` extension — text/terminal visualization —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `plot_sparkline(nums_json) -> text`     unicode block sparkline
//!   * `plot_bars(nums_json, width) -> text`   multi-line horizontal bar chart
//!   * `qr_utf8(text) -> text`                 compact UTF-8 QR rendering
//!
//! `NULL` / invalid input -> `NULL`. Never panics.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Parse a JSON array of numbers into a Vec<f64>. None on any failure.
    pub fn parse_nums(s: &str) -> Option<Vec<f64>> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        let arr = v.as_array()?;
        if arr.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let n = item.as_f64()?;
            if !n.is_finite() {
                return None;
            }
            out.push(n);
        }
        Some(out)
    }

    const BLOCKS: [char; 8] = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];

    pub fn sparkline(nums: &[f64]) -> String {
        let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        let mut s = String::with_capacity(nums.len() * 3);
        for &n in nums {
            let idx = if range <= 0.0 {
                0
            } else {
                let frac = (n - min) / range;
                ((frac * 7.0).round() as usize).min(7)
            };
            s.push(BLOCKS[idx]);
        }
        s
    }

    pub fn bars(nums: &[f64], width: i64) -> Option<String> {
        if width <= 0 {
            return None;
        }
        let w = width as usize;
        let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let top = max.max(0.0);
        let bottom = min.min(0.0);
        let span = top - bottom;
        let mut lines = Vec::with_capacity(nums.len());
        for &n in nums {
            let frac = if span <= 0.0 { 0.0 } else { (n - bottom) / span };
            let count = (frac * w as f64).round() as usize;
            let count = count.min(w);
            lines.push("#".repeat(count));
        }
        Some(lines.join("\n"))
    }

    pub fn qr(text: &str) -> Option<String> {
        use qrcode::render::unicode;
        use qrcode::QrCode;
        let code = QrCode::new(text.as_bytes()).ok()?;
        let s = code
            .render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Dark)
            .light_color(unicode::Dense1x2::Light)
            .quiet_zone(false)
            .build();
        Some(s)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "textplot";
    version = env!("CARGO_PKG_VERSION");

    scalar plot_sparkline(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "plot_sparkline")?;
        Ok(match logic::parse_nums(&s) {
            Some(nums) => NeutralValue::Text(logic::sparkline(&nums)),
            None => NeutralValue::Null,
        })
    };

    scalar plot_bars(text, int64) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "plot_bars")?;
        let w = args.arg_int(1, "plot_bars")?;
        Ok(match logic::parse_nums(&s) {
            Some(nums) => match logic::bars(&nums, w) {
                Some(out) => NeutralValue::Text(out),
                None => NeutralValue::Null,
            },
            None => NeutralValue::Null,
        })
    };

    scalar qr_utf8(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "qr_utf8")?;
        Ok(match logic::qr(&s) {
            Some(out) => NeutralValue::Text(out),
            None => NeutralValue::Null,
        })
    };
}
