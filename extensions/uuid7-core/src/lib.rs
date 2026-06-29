//! Neutral core for the `uuid7` extension — UUIDv7 (RFC 9562, time-ordered) —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! All functions are DETERMINISTIC (timestamp + random bits are supplied):
//!   * `uuid7_build(unix_ms, rand_hex) -> text`   canonical v7 from ms + hex
//!   * `uuid7_timestamp(uuid) -> int64`           embedded unix-ms (NULL invalid)
//!   * `uuid7_is_valid(uuid) -> boolean`          well-formed v7? (CALLED; NULL -> false)
//!
//! Parse failures -> `NULL` / false. Never panics.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use uuid::Uuid;

    /// Parse a hex string into a 74-bit random field (zero-padded if short,
    /// truncated to the low 74 bits if long). None if any char is not hex.
    pub fn rand_bits_from_hex(hex: &str) -> Option<u128> {
        let mut acc: u128 = 0;
        for c in hex.chars() {
            let nib = c.to_digit(16)?;
            acc = (acc << 4) | nib as u128;
            acc &= (1u128 << 74) - 1;
        }
        Some(acc)
    }

    /// Build the canonical v7 UUID string from a 48-bit timestamp + 74 random bits.
    pub fn build_v7(unix_ms: u64, rand74: u128) -> Option<String> {
        if unix_ms > 0xFFFF_FFFF_FFFF {
            return None; // does not fit in 48 bits
        }
        let rand_a: u16 = ((rand74 >> 62) & 0xFFF) as u16;
        let rand_b: u64 = (rand74 & ((1u128 << 62) - 1)) as u64;

        let mut bytes = [0u8; 16];
        bytes[0] = (unix_ms >> 40) as u8;
        bytes[1] = (unix_ms >> 32) as u8;
        bytes[2] = (unix_ms >> 24) as u8;
        bytes[3] = (unix_ms >> 16) as u8;
        bytes[4] = (unix_ms >> 8) as u8;
        bytes[5] = unix_ms as u8;
        bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F);
        bytes[7] = (rand_a & 0xFF) as u8;
        bytes[8] = 0x80 | ((rand_b >> 56) as u8 & 0x3F);
        bytes[9] = (rand_b >> 48) as u8;
        bytes[10] = (rand_b >> 40) as u8;
        bytes[11] = (rand_b >> 32) as u8;
        bytes[12] = (rand_b >> 24) as u8;
        bytes[13] = (rand_b >> 16) as u8;
        bytes[14] = (rand_b >> 8) as u8;
        bytes[15] = rand_b as u8;

        Some(Uuid::from_bytes(bytes).to_string())
    }

    /// Is this a well-formed v7 UUID (version nibble 7, variant 0b10)?
    pub fn is_v7(s: &str) -> bool {
        match Uuid::parse_str(s) {
            Ok(u) => {
                let b = u.as_bytes();
                let version = b[6] >> 4;
                let variant = b[8] >> 6;
                version == 7 && variant == 0b10
            }
            Err(_) => false,
        }
    }

    /// Extract the embedded unix-ms timestamp from a v7 UUID, or None.
    pub fn v7_timestamp_ms(s: &str) -> Option<i64> {
        let u = Uuid::parse_str(s).ok()?;
        if !is_v7(s) {
            return None;
        }
        let b = u.as_bytes();
        let ms: u64 = (b[0] as u64) << 40
            | (b[1] as u64) << 32
            | (b[2] as u64) << 24
            | (b[3] as u64) << 16
            | (b[4] as u64) << 8
            | (b[5] as u64);
        i64::try_from(ms).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "uuid7";
    version = env!("CARGO_PKG_VERSION");

    scalar uuid7_build(int64, text) -> text [propagate, deterministic] = |args| {
        let ms = args.arg_int(0, "uuid7_build")?;
        let hex = args.arg_text(1, "uuid7_build")?;
        if ms < 0 {
            return Ok(NeutralValue::Null);
        }
        Ok(
            match logic::rand_bits_from_hex(&hex).and_then(|r| logic::build_v7(ms as u64, r)) {
                Some(s) => NeutralValue::Text(s),
                None => NeutralValue::Null,
            },
        )
    };

    scalar uuid7_timestamp(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "uuid7_timestamp")?;
        Ok(match logic::v7_timestamp_ms(&s) {
            Some(ms) => NeutralValue::Int64(ms),
            None => NeutralValue::Null,
        })
    };

    scalar uuid7_is_valid(text) -> boolean [called, deterministic] = |args| {
        let s = args.arg_text(0, "uuid7_is_valid")?;
        Ok(NeutralValue::Boolean(logic::is_v7(&s)))
    };
}
