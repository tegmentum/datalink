//! Neutral core for the `rle` extension — run-length encoding of text —
//! written ONCE. The per-DB shims are generated from the [`declare!`] table.
//!
//! # Functions
//!   * `rle_encode(text) -> text` — "aaabbc" -> "3a2b1c".
//!   * `rle_decode(text) -> text` — inverse; NULL on malformed input.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    /// Run-length encode: each maximal run of a char `c` of length `n`
    /// becomes "<n><c>". "aaabbc" -> "3a2b1c".
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let mut n = 1;
            while i + n < chars.len() && chars[i + n] == c {
                n += 1;
            }
            out.push_str(&n.to_string());
            out.push(c);
            i += n;
        }
        out
    }

    /// Inverse of [`encode`]; `None` on malformed input (a char with no
    /// leading count, or a trailing dangling count).
    pub fn decode(s: &str) -> Option<String> {
        let mut out = String::new();
        let mut num = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                if num.is_empty() {
                    return None;
                }
                let n: usize = num.parse().ok()?;
                for _ in 0..n {
                    out.push(c);
                }
                num.clear();
            }
        }
        if num.is_empty() {
            Some(out)
        } else {
            None
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "rle";
    version = env!("CARGO_PKG_VERSION");

    scalar rle_encode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "rle_encode")?;
        Ok(NeutralValue::Text(logic::encode(&s)))
    };

    scalar rle_decode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "rle_decode")?;
        Ok(match logic::decode(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::string::String;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn roundtrip() {
        assert_eq!(
            Core::dispatch(idx("rle_encode"), &[t("aaabbc")]).unwrap(),
            t("3a2b1c")
        );
        assert_eq!(
            Core::dispatch(idx("rle_decode"), &[t("3a2b1c")]).unwrap(),
            t("aaabbc")
        );
        assert_eq!(
            Core::dispatch(idx("rle_decode"), &[t("abc")]).unwrap(),
            NeutralValue::Null
        );
    }
}
