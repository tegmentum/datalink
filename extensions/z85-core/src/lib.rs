//! Neutral core for the `z85` extension — Z85 (ZeroMQ Base85) codec — written
//! ONCE. The per-DB shims (ducklink `duckdb:extension`, sqlink
//! `sqlite:extension`, sqlink embed) are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `z85_encode(hex) -> text` — hex-decode the input, then Z85-encode the
//!     bytes (length must be a multiple of 4); invalid -> NULL.
//!   * `z85_decode(text) -> hex` — Z85-decode the input, then hex-encode the
//!     bytes; invalid -> NULL.

extern crate alloc;

use datalink_extcore::{ArgExt, NeutralValue};

/// Logic, byte-for-byte the pre-pullup helpers (DB-agnostic).
pub mod logic {
    pub const HEX: &[u8] = b"0123456789abcdef";

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
}

datalink_extcore::declare! {
    core = Core;
    extension = "z85";
    version = env!("CARGO_PKG_VERSION");

    scalar z85_encode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "z85_encode")?;
        Ok(match logic::hex_decode(&s) {
            Some(b) if b.len() % 4 == 0 => NeutralValue::Text(z85::encode(&b)),
            _ => NeutralValue::Null,
        })
    };

    scalar z85_decode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "z85_decode")?;
        Ok(match z85::decode(s.trim()) {
            Ok(b) => NeutralValue::Text(logic::hex_encode(&b)),
            Err(_) => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn roundtrip() {
        // 8 hex bytes (multiple of 4) -> z85 -> back to the same hex.
        let enc = Core::dispatch(idx("z85_encode"), &[t("86 4f d2 6f b5 59 f7 5b".replace(' ', "").as_str())]).unwrap();
        let ct = match &enc {
            NeutralValue::Text(s) => s.clone(),
            o => panic!("{o:?}"),
        };
        let dec = Core::dispatch(idx("z85_decode"), &[NeutralValue::Text(ct)]).unwrap();
        assert_eq!(dec, t("864fd26fb559f75b"));
    }

    #[test]
    fn bad_length_is_null() {
        // 2 bytes is not a multiple of 4 -> NULL.
        assert_eq!(
            Core::dispatch(idx("z85_encode"), &[t("abcd")]).unwrap(),
            NeutralValue::Null
        );
    }
}
