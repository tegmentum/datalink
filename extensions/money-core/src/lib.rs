//! Neutral core for the `money` extension — currency amount formatting
//! via the `iso_currency` crate — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `format_money(amount, currency_code) -> text` — e.g. "$1,234.50",
//!     "¥1,000". Uses the currency's symbol and minor-unit count;
//!     thousands-grouped. Unknown currency -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use iso_currency::Currency;

/// Logic helper: thousands-group an integer-part string (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    pub fn group(int_part: &str) -> String {
        let bytes = int_part.as_bytes();
        let mut out = String::new();
        let n = bytes.len();
        for (i, &b) in bytes.iter().enumerate() {
            if i > 0 && (n - i) % 3 == 0 {
                out.push(',');
            }
            out.push(b as char);
        }
        out
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "money";
    version = env!("CARGO_PKG_VERSION");

    scalar format_money(float64, text) -> text [propagate, deterministic] = |args| {
        let amount = args.arg_float(0, "format_money")?;
        let code = args.arg_text(1, "format_money")?;
        let cur = match Currency::from_code(&code.trim().to_ascii_uppercase()) {
            Some(c) => c,
            None => return Ok(NeutralValue::Null),
        };
        let decimals = cur.exponent().unwrap_or(2) as usize;
        let neg = amount.is_sign_negative();
        let formatted = alloc::format!("{:.*}", decimals, amount.abs());
        let (int_part, frac_part) = match formatted.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (formatted.as_str(), None),
        };
        let mut s: String = alloc::format!("{}{}", cur.symbol(), logic::group(int_part));
        if let Some(f) = frac_part {
            s.push('.');
            s.push_str(f);
        }
        if neg {
            s.insert(0, '-');
        }
        Ok(NeutralValue::Text(s))
    };
}
