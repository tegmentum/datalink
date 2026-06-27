//! Neutral core for the `crypto` extension — cryptographic + checksum
//! hash digests beyond DuckDB's built-in md5/sha256 — written ONCE. The
//! per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `sha1 / sha512 / sha3_256 / blake3(value) -> text`  (hex digest)
//!   * `crc32(value) -> int64`  (CRC-32 IEEE checksum)
//!
//! `value` is hashed as UTF-8 bytes (TEXT) or raw bytes (BLOB);
//! `NULL -> NULL`. The surface is identical in both ports (zero drift).

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Hashing (DB-agnostic). Each takes raw bytes and returns the hex
/// digest (or the integer checksum for crc32).
pub mod logic {
    use alloc::string::String;
    use sha1::Sha1;
    use sha2::{Digest, Sha512};
    use sha3::Sha3_256;

    pub fn to_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }
    pub fn sha1_hex(b: &[u8]) -> String {
        let mut h = Sha1::new();
        h.update(b);
        to_hex(&h.finalize())
    }
    pub fn sha512_hex(b: &[u8]) -> String {
        let mut h = Sha512::new();
        h.update(b);
        to_hex(&h.finalize())
    }
    pub fn sha3_256_hex(b: &[u8]) -> String {
        let mut h = Sha3_256::new();
        h.update(b);
        to_hex(&h.finalize())
    }
    pub fn blake3_hex(b: &[u8]) -> String {
        blake3::hash(b).to_hex().to_string()
    }
    pub fn crc32_of(b: &[u8]) -> u32 {
        crc32fast::hash(b)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "crypto";
    version = env!("CARGO_PKG_VERSION");

    scalar sha1(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::sha1_hex(&args.arg_blob(0, "sha1")?)))
    };
    scalar sha512(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::sha512_hex(&args.arg_blob(0, "sha512")?)))
    };
    scalar sha3_256(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::sha3_256_hex(&args.arg_blob(0, "sha3_256")?)))
    };
    scalar blake3(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::blake3_hex(&args.arg_blob(0, "blake3")?)))
    };
    scalar crc32(text) -> int64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Int64(logic::crc32_of(&args.arg_blob(0, "crc32")?) as i64))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue { NeutralValue::Text(alloc::string::String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(
            Core::dispatch(idx("sha1"), &[t("abc")]).unwrap(),
            t("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        assert_eq!(
            Core::dispatch(idx("crc32"), &[t("abc")]).unwrap(),
            NeutralValue::Int64(crc32fast::hash(b"abc") as i64)
        );
    }
}
