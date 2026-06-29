//! Neutral core for the `tsid` extension — time-sorted unique IDs
//! (Snowflake / TSID style) — written ONCE. The per-DB shims are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//! All functions are DETERMINISTIC (no clock / RNG):
//!   * `tsid_encode(int64) -> text`   Crockford base-32 of the 64-bit id
//!   * `tsid_decode(text) -> int64`   parse base-32 back to i64 (NULL on invalid)
//!   * `tsid_timestamp(int64) -> int64`   (id >> 22) + CUSTOM_EPOCH_MS
//!   * `tsid_from_timestamp(int64) -> int64`   ((ms - CUSTOM_EPOCH_MS) << 22)

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// TSID standard custom epoch: 2020-01-01T00:00:00Z in milliseconds.
    pub const CUSTOM_EPOCH_MS: i64 = 1_577_836_800_000;

    /// Crockford base-32 alphabet (excludes I, L, O, U).
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// Encode a 64-bit id as a 13-char Crockford base-32 string (MSB group first).
    pub fn encode_crockford(id: i64) -> String {
        let u = id as u64;
        let mut buf = [0u8; 13];
        for i in (0..13).rev() {
            let shift = (12 - i) * 5;
            let idx = ((u >> shift) & 0x1f) as usize;
            buf[i] = CROCKFORD[idx];
        }
        String::from_utf8(buf.to_vec()).unwrap()
    }

    /// Parse a Crockford base-32 string back to a 64-bit id. None on any invalid
    /// character. Case-insensitive; I/L map to 1, O maps to 0 (Crockford leniency).
    pub fn decode_crockford(s: &str) -> Option<i64> {
        let s = s.trim();
        if s.is_empty() || s.len() > 13 {
            return None;
        }
        let mut acc: u64 = 0;
        for ch in s.bytes() {
            let v: u64 = match ch.to_ascii_uppercase() {
                b'0' | b'O' => 0,
                b'1' | b'I' | b'L' => 1,
                c @ b'2'..=b'9' => (c - b'0') as u64,
                c @ b'A'..=b'H' => (c - b'A' + 10) as u64,
                b'J' => 18,
                b'K' => 19,
                b'M' => 20,
                b'N' => 21,
                c @ b'P'..=b'T' => (c - b'P' + 22) as u64,
                c @ b'V'..=b'Z' => (c - b'V' + 27) as u64,
                _ => return None,
            };
            acc = acc.checked_mul(32)?.checked_add(v)?;
        }
        Some(acc as i64)
    }

    #[allow(dead_code)]
    fn _use_vec(_: Vec<u8>) {}
}

datalink_extcore::declare! {
    core = Core;
    extension = "tsid";
    version = env!("CARGO_PKG_VERSION");

    scalar tsid_encode(int64) -> text [propagate, deterministic] = |args| {
        let id = args.arg_int(0, "tsid_encode")?;
        Ok(NeutralValue::Text(logic::encode_crockford(id)))
    };

    scalar tsid_decode(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "tsid_decode")?;
        Ok(match logic::decode_crockford(&s) {
            Some(id) => NeutralValue::Int64(id),
            None => NeutralValue::Null,
        })
    };

    scalar tsid_timestamp(int64) -> int64 [propagate, deterministic] = |args| {
        let id = args.arg_int(0, "tsid_timestamp")?;
        Ok(NeutralValue::Int64(((id as u64 >> 22) as i64) + logic::CUSTOM_EPOCH_MS))
    };

    scalar tsid_from_timestamp(int64) -> int64 [propagate, deterministic] = |args| {
        let ms = args.arg_int(0, "tsid_from_timestamp")?;
        let rel = (ms - logic::CUSTOM_EPOCH_MS) as u64;
        Ok(NeutralValue::Int64((rel << 22) as i64))
    };
}
