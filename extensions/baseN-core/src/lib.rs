//! Neutral core for the `baseN` extension — compact base-N codecs —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions (BLOB <-> TEXT)
//!
//!   * `base32_encode(blob) -> text`  RFC 4648, no padding
//!   * `base32_decode(text) -> blob`  NULL on invalid input
//!   * `base58_encode(blob) -> text`  Bitcoin alphabet
//!   * `base58_decode(text) -> blob`  NULL on invalid input
//!   * `to_base64(blob) -> text`      RFC 4648 standard base64
//!   * `from_base64(text) -> blob`    NULL on invalid input
//!
//! # Reconciled drift
//!
//! Before the pull-up ducklink shipped 4 functions (base32/base58 only)
//! and sqlink shipped 6 (adding `to_base64`/`from_base64`). This core
//! adopts the 6-function SUPERSET, so ducklink gains base64 — the
//! write-once win.
//!
//! # Marshalling subtleties exercised here
//!
//!   * BLOB<->TEXT: encoders take a blob, return text; decoders the
//!     reverse. The arg helpers accept the cross-type fallthrough the
//!     pre-pullup extensions had (text bytes as blob, blob utf8 as text).
//!   * Option->Null: a decode failure returns
//!     [`NeutralValue::Null`](datalink_extcore::NeutralValue) rather than
//!     raising, so `COALESCE(base58_decode(x), default)` composes.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

const B32: base32::Alphabet = base32::Alphabet::Rfc4648 { padding: false };

/// Logic (DB-agnostic). Encoders are infallible; decoders return
/// `Option`/`Result` so the shim can map failure to SQL NULL.
pub mod logic {
    use super::B32;
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn base32_encode(b: &[u8]) -> String {
        base32::encode(B32, b)
    }
    pub fn base32_decode(t: &str) -> Option<Vec<u8>> {
        base32::decode(B32, t)
    }
    pub fn base58_encode(b: &[u8]) -> String {
        bs58::encode(b).into_string()
    }
    pub fn base58_decode(t: &str) -> Option<Vec<u8>> {
        bs58::decode(t.as_bytes()).into_vec().ok()
    }
    pub fn to_base64(b: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(b)
    }
    pub fn from_base64(t: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(t.as_bytes())
            .ok()
    }
}

/// Map an optional decode result to a neutral value (NULL on failure).
fn opt_blob(o: Option<alloc::vec::Vec<u8>>) -> NeutralValue {
    match o {
        Some(b) => NeutralValue::Blob(b),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "baseN";
    version = env!("CARGO_PKG_VERSION");

    scalar base32_encode(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "base32_encode")?;
        Ok(NeutralValue::Text(logic::base32_encode(&b)))
    };
    scalar base32_decode(text) -> blob [propagate, deterministic] = |args| {
        let t = args.arg_text(0, "base32_decode")?;
        Ok(opt_blob(logic::base32_decode(&t)))
    };
    scalar base58_encode(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "base58_encode")?;
        Ok(NeutralValue::Text(logic::base58_encode(&b)))
    };
    scalar base58_decode(text) -> blob [propagate, deterministic] = |args| {
        let t = args.arg_text(0, "base58_decode")?;
        Ok(opt_blob(logic::base58_decode(&t)))
    };
    scalar to_base64(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "to_base64")?;
        Ok(NeutralValue::Text(logic::to_base64(&b)))
    };
    scalar from_base64(text) -> blob [propagate, deterministic] = |args| {
        let t = args.arg_text(0, "from_base64")?;
        Ok(opt_blob(logic::from_base64(&t)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(alloc::string::String::from(s))
    }
    fn b(bytes: &[u8]) -> NeutralValue {
        NeutralValue::Blob(bytes.to_vec())
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn declares_the_reconciled_superset() {
        let names: std::vec::Vec<_> = Core::DECLS.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "base32_encode",
                "base32_decode",
                "base58_encode",
                "base58_decode",
                "to_base64",
                "from_base64"
            ]
        );
    }

    #[test]
    fn base32_roundtrip_and_text_arg() {
        assert_eq!(
            Core::dispatch(idx("base32_encode"), &[b(b"Hello")]).unwrap(),
            NeutralValue::Text(alloc::string::String::from("JBSWY3DP"))
        );
        // base32_encode accepts a TEXT arg (arg_blob TEXT fallthrough).
        assert_eq!(
            Core::dispatch(idx("base32_encode"), &[t("Hello")]).unwrap(),
            NeutralValue::Text(alloc::string::String::from("JBSWY3DP"))
        );
        assert_eq!(
            Core::dispatch(idx("base32_decode"), &[t("JBSWY3DP")]).unwrap(),
            b(b"Hello")
        );
    }

    #[test]
    fn decode_error_is_null() {
        // base58 alphabet excludes '0','I','l' -> Option::None -> NULL.
        assert_eq!(
            Core::dispatch(idx("base58_decode"), &[t("invalid0Il")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn base64_superset_present() {
        assert_eq!(
            Core::dispatch(idx("to_base64"), &[b(b"Hello")]).unwrap(),
            NeutralValue::Text(alloc::string::String::from("SGVsbG8="))
        );
        assert_eq!(
            Core::dispatch(idx("from_base64"), &[t("SGVsbG8=")]).unwrap(),
            b(b"Hello")
        );
    }
}
