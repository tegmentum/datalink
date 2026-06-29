//! Neutral core for the `msgpack` extension — JSON <-> MessagePack
//! (bridged through `serde_json::Value`, hex-encoded) — written ONCE. The
//! per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `msgpack_from_json(json) -> text` (MessagePack hex)
//!   * `msgpack_to_json(hex) -> text`
//!
//! Invalid input -> NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup codecs (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    pub fn json_to_mp_hex(json: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        Some(hex::encode(rmp_serde::to_vec(&v).ok()?))
    }

    pub fn mp_hex_to_json(h: &str) -> Option<String> {
        let bytes = hex::decode(h.trim()).ok()?;
        let v: serde_json::Value = rmp_serde::from_slice(&bytes).ok()?;
        serde_json::to_string(&v).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "msgpack";
    version = env!("CARGO_PKG_VERSION");

    scalar msgpack_from_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "msgpack_from_json")?;
        Ok(match logic::json_to_mp_hex(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };

    scalar msgpack_to_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "msgpack_to_json")?;
        Ok(match logic::mp_hex_to_json(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}
