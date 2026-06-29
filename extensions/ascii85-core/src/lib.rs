//! Neutral core for the `ascii85` extension — Ascii85 / Base85 (via the
//! `ascii85` crate) — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `ascii85_encode(text) -> text`
//!   * `ascii85_decode(text) -> text` (invalid / non-UTF-8 -> NULL)
//!
//! Not `#![no_std]`: the `ascii85` crate is consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn encode(bytes: &[u8]) -> String {
        ascii85::encode(bytes)
    }

    /// Decode then interpret the bytes as UTF-8; `None` on any failure.
    pub fn decode_to_text(s: &str) -> Option<String> {
        let bytes: Vec<u8> = ascii85::decode(s).ok()?;
        String::from_utf8(bytes).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ascii85";
    version = env!("CARGO_PKG_VERSION");

    scalar ascii85_encode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ascii85_encode")?;
        Ok(NeutralValue::Text(logic::encode(s.as_bytes())))
    };

    scalar ascii85_decode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ascii85_decode")?;
        Ok(match logic::decode_to_text(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}
