//! Neutral core for the `cardtype` extension — credit-card brand detection
//! by IIN range (visa / mastercard / amex / discover / diners / jcb /
//! unionpay / maestro / unknown; non-digits stripped) — written ONCE. The
//! per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `card_brand(number) -> text`

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    fn prefix(d: &str, n: usize) -> u32 {
        d.get(..n).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn brand(raw: &str) -> &'static str {
        let d: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        let len = d.len();
        if !(12..=19).contains(&len) {
            return "unknown";
        }
        let p2 = prefix(&d, 2);
        let p4 = prefix(&d, 4);
        let p6 = prefix(&d, 6);
        let p3 = prefix(&d, 3);
        if d.starts_with('4') {
            "visa"
        } else if (p2 == 34 || p2 == 37) && len == 15 {
            "amex"
        } else if (51..=55).contains(&p2) || (2221..=2720).contains(&p4) {
            "mastercard"
        } else if p4 == 6011 || p2 == 65 || (644..=649).contains(&p3) || (622126..=622925).contains(&p6) {
            "discover"
        } else if (3528..=3589).contains(&p4) {
            "jcb"
        } else if (300..=305).contains(&p3) || p3 == 309 || p2 == 36 || p2 == 38 {
            "diners"
        } else if p2 == 62 {
            "unionpay"
        } else if p2 == 50 || (56..=69).contains(&p2) {
            "maestro"
        } else {
            "unknown"
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "cardtype";
    version = env!("CARGO_PKG_VERSION");

    scalar card_brand(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "card_brand")?;
        Ok(NeutralValue::Text(::alloc::string::String::from(logic::brand(&raw))))
    };
}
