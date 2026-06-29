//! Neutral core for the `base58check` extension — Base58Check (via the `bs58`
//! crate, the 4-byte checksum used by Bitcoin-style addresses) — written
//! ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `base58check_encode(hex) -> text` (invalid hex -> NULL)
//!   * `base58check_decode(text) -> hex`  (invalid / bad checksum -> NULL)
//!
//! Not `#![no_std]`: the `bs58` crate is consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    const HEX: &[u8] = b"0123456789abcdef";

    pub fn hex_encode(b: &[u8]) -> String {
        let mut o = String::with_capacity(b.len() * 2);
        for &x in b {
            o.push(HEX[(x >> 4) as usize] as char);
            o.push(HEX[(x & 0xf) as usize] as char);
        }
        o
    }

    pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
        let s = s.trim();
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect()
    }

    /// hex -> Base58Check string; `None` if the hex is invalid.
    pub fn encode(hex: &str) -> Option<String> {
        let b = hex_decode(hex)?;
        Some(bs58::encode(b).with_check().into_string())
    }

    /// Base58Check -> hex; `None` on a bad checksum / invalid input.
    pub fn decode(s: &str) -> Option<String> {
        let b = bs58::decode(s.trim()).with_check(None).into_vec().ok()?;
        Some(hex_encode(&b))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "base58check";
    version = env!("CARGO_PKG_VERSION");

    scalar base58check_encode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "base58check_encode")?;
        Ok(match logic::encode(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };

    scalar base58check_decode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "base58check_decode")?;
        Ok(match logic::decode(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}
