//! Neutral core for the `vigenere` extension — the Vigenere polyalphabetic
//! cipher — written ONCE. The per-DB shims (ducklink `duckdb:extension`,
//! sqlink `sqlite:extension`, sqlink embed) are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `vigenere_encrypt(text, key) -> text`
//!   * `vigenere_decrypt(text, key) -> text`
//!
//! Only ASCII letters are enciphered (case preserved); every other char
//! passes through. The key cycles over its letters only. NULL args
//! propagate to NULL; a key with no letters yields NULL.

#![no_std]

extern crate alloc;

use datalink_extcore::{ArgExt, NeutralValue};

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Vigenere cipher. `None` if the key has no ASCII letters.
    pub fn cipher(text: &str, key: &str, decrypt: bool) -> Option<String> {
        let shifts: Vec<u8> = key
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .map(|c| c.to_ascii_lowercase() as u8 - b'a')
            .collect();
        if shifts.is_empty() {
            return None;
        }
        let mut ki = 0;
        Some(
            text.chars()
                .map(|c| {
                    let base = if c.is_ascii_uppercase() {
                        b'A'
                    } else if c.is_ascii_lowercase() {
                        b'a'
                    } else {
                        return c;
                    };
                    let k = shifts[ki % shifts.len()];
                    ki += 1;
                    let k = if decrypt { 26 - k } else { k };
                    (((c as u8 - base + k) % 26) + base) as char
                })
                .collect(),
        )
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "vigenere";
    version = env!("CARGO_PKG_VERSION");

    scalar vigenere_encrypt(text, text) -> text [propagate, deterministic] = |args| {
        let text = args.arg_text(0, "vigenere_encrypt")?;
        let key = args.arg_text(1, "vigenere_encrypt")?;
        Ok(match logic::cipher(&text, &key, false) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar vigenere_decrypt(text, text) -> text [propagate, deterministic] = |args| {
        let text = args.arg_text(0, "vigenere_decrypt")?;
        let key = args.arg_text(1, "vigenere_decrypt")?;
        Ok(match logic::cipher(&text, &key, true) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn roundtrip() {
        let enc = Core::dispatch(idx("vigenere_encrypt"), &[t("Hello, World!"), t("key")]).unwrap();
        let ct = match &enc {
            NeutralValue::Text(s) => s.clone(),
            o => panic!("{o:?}"),
        };
        let dec = Core::dispatch(idx("vigenere_decrypt"), &[NeutralValue::Text(ct), t("key")]).unwrap();
        assert_eq!(dec, t("Hello, World!"));
    }

    #[test]
    fn empty_key_is_null() {
        assert_eq!(
            Core::dispatch(idx("vigenere_encrypt"), &[t("abc"), t("123")]).unwrap(),
            NeutralValue::Null
        );
    }
}
