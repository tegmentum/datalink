//! Neutral core for the `atbash` extension — the Atbash substitution
//! cipher (a<->z, b<->y, ... within each ASCII case; non-letters pass
//! through; self-inverse) — written ONCE. The per-DB shims are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `atbash(text) -> text`

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    pub fn atbash(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    (b'Z' - (c as u8 - b'A')) as char
                } else if c.is_ascii_lowercase() {
                    (b'z' - (c as u8 - b'a')) as char
                } else {
                    c
                }
            })
            .collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "atbash";
    version = env!("CARGO_PKG_VERSION");

    scalar atbash(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "atbash")?;
        Ok(NeutralValue::Text(logic::atbash(&s)))
    };
}
