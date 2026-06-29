//! Neutral core for the `bech32` extension — BIP-173 bech32 (via the `bech32`
//! crate) — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `bech32_encode(hrp, hex) -> text`  (invalid -> NULL)
//!   * `bech32_hrp(text) -> text`         (invalid -> NULL)
//!   * `bech32_decode_hex(text) -> text`  (invalid -> NULL)
//!   * `bech32_valid(text) -> boolean`    (NULL / invalid -> false)
//!
//! Not `#![no_std]`: the `bech32` crate is consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use bech32::{Bech32, Hrp};

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

    /// (hrp, hex) -> bech32 string; `None` on any parse / encode failure.
    pub fn encode(hrp: &str, hex: &str) -> Option<String> {
        let hrp = Hrp::parse(hrp.trim()).ok()?;
        let data = hex_decode(hex)?;
        bech32::encode::<Bech32>(hrp, &data).ok()
    }

    pub fn valid(s: &str) -> bool {
        bech32::decode(s.trim()).is_ok()
    }

    /// The human-readable part of a bech32 string; `None` if it does not decode.
    pub fn hrp(s: &str) -> Option<String> {
        bech32::decode(s.trim()).ok().map(|(hrp, _)| hrp.to_string())
    }

    /// The data bytes of a bech32 string as hex; `None` if it does not decode.
    pub fn decode_hex(s: &str) -> Option<String> {
        bech32::decode(s.trim()).ok().map(|(_, data)| hex_encode(&data))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "bech32";
    version = env!("CARGO_PKG_VERSION");

    scalar bech32_encode(text, text) -> text [propagate, deterministic] = |args| {
        let hrp = args.arg_text(0, "bech32_encode")?;
        let hex = args.arg_text(1, "bech32_encode")?;
        Ok(match logic::encode(&hrp, &hex) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar bech32_hrp(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "bech32_hrp")?;
        Ok(match logic::hrp(&s) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar bech32_decode_hex(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "bech32_decode_hex")?;
        Ok(match logic::decode_hex(&s) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    // NULL / invalid -> false (matches the pre-pullup `unwrap_or(false)`), so
    // this one is `called` (it must see NULL, not be pre-filtered to NULL).
    scalar bech32_valid(text) -> boolean [called, deterministic] = |args| {
        let s = args.arg_text(0, "bech32_valid")?;
        Ok(NeutralValue::Boolean(logic::valid(&s)))
    };
}
