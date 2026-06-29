//! Neutral core for the `ngrams` extension — character / word n-grams
//! emitted as a JSON array — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `char_ngrams(text, n int64) -> text` (JSON array)
//!   * `word_ngrams(text, n int64) -> text` (JSON array)
//!
//! `n < 1` (or longer than the input) yields "[]"... actually `n < 1`
//! yields NULL (matching the pre-pullup behaviour); `n` longer than the
//! input yields "[]". NULL args propagate.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn char_ngrams(text: &str, n: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < n {
            Vec::new()
        } else {
            chars.windows(n).map(|w| w.iter().collect()).collect()
        }
    }

    pub fn word_ngrams(text: &str, n: usize) -> Vec<String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() < n {
            Vec::new()
        } else {
            words.windows(n).map(|w| w.join(" ")).collect()
        }
    }
}

fn to_json(grams: Vec<String>) -> NeutralValue {
    NeutralValue::Text(serde_json::to_string(&grams).unwrap_or_else(|_| String::from("[]")))
}

datalink_extcore::declare! {
    core = Core;
    extension = "ngrams";
    version = env!("CARGO_PKG_VERSION");

    scalar char_ngrams(text, int64) -> text [propagate, deterministic] = |args| {
        let text = args.arg_text(0, "char_ngrams")?;
        let n = args.arg_int(1, "char_ngrams")?;
        if n < 1 {
            return Ok(NeutralValue::Null);
        }
        Ok(to_json(logic::char_ngrams(&text, n as usize)))
    };

    scalar word_ngrams(text, int64) -> text [propagate, deterministic] = |args| {
        let text = args.arg_text(0, "word_ngrams")?;
        let n = args.arg_int(1, "word_ngrams")?;
        if n < 1 {
            return Ok(NeutralValue::Null);
        }
        Ok(to_json(logic::word_ngrams(&text, n as usize)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    #[test]
    fn bigrams_and_too_long() {
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Text(String::from("hello")), NeutralValue::Int64(2)]).unwrap(),
            NeutralValue::Text(String::from(r#"["he","el","ll","lo"]"#))
        );
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Text(String::from("hi")), NeutralValue::Int64(5)]).unwrap(),
            NeutralValue::Text(String::from("[]"))
        );
    }
}
