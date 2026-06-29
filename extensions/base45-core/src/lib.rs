//! Neutral core for the `base45` extension — RFC 9285 Base45 codec (via the
//! `base45` crate), the EU Digital COVID Certificate transport encoding —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `base45_encode(blob) -> text`
//!   * `base45_decode(text) -> blob` (invalid -> NULL)
//!
//! Not `#![no_std]`: the `base45` crate is consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn encode(bytes: &[u8]) -> String {
        base45::encode(bytes)
    }

    /// Decode Base45 text to bytes; `None` on invalid input.
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        base45::decode(s).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "base45";
    version = env!("CARGO_PKG_VERSION");

    scalar base45_encode(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "base45_encode")?;
        Ok(NeutralValue::Text(logic::encode(&b)))
    };

    scalar base45_decode(text) -> blob [propagate, deterministic] = |args| {
        let t = args.arg_text(0, "base45_decode")?;
        Ok(match logic::decode(&t) {
            Some(b) => NeutralValue::Blob(b),
            None => NeutralValue::Null,
        })
    };
}
