//! Neutral core for the `cbor` extension — JSON <-> CBOR (via `ciborium`,
//! bridged through `serde_json::Value`) — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `cbor_from_json(json) -> text` (hex of the CBOR bytes; invalid -> NULL)
//!   * `cbor_to_json(hex) -> text`    (invalid -> NULL)
//!
//! Not `#![no_std]`: `serde_json` / `ciborium` / `hex` are consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// JSON text -> hex of the CBOR encoding; `None` on invalid JSON.
    pub fn json_to_cbor_hex(json: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let mut buf = Vec::new();
        ciborium::into_writer(&v, &mut buf).ok()?;
        Some(hex::encode(buf))
    }

    /// CBOR hex -> JSON text; `None` on invalid hex / CBOR.
    pub fn cbor_hex_to_json(h: &str) -> Option<String> {
        let bytes = hex::decode(h.trim()).ok()?;
        let v: serde_json::Value = ciborium::from_reader(&bytes[..]).ok()?;
        serde_json::to_string(&v).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "cbor";
    version = env!("CARGO_PKG_VERSION");

    scalar cbor_from_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "cbor_from_json")?;
        Ok(match logic::json_to_cbor_hex(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };

    scalar cbor_to_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "cbor_to_json")?;
        Ok(match logic::cbor_hex_to_json(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}
