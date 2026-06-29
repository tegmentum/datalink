//! Neutral core for the `textstat` extension — readability statistics —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `word_count / sentence_count / syllable_count(text) -> int64`
//!   * `flesch_reading_ease(text) -> float64`  (higher = easier; NULL when
//!     undefined, i.e. no words or no sentences)
//!   * `reading_time_minutes(text) -> float64`  (at 200 wpm)
//!
//! `NULL -> NULL`. Hand-rolled (no external crate). Byte-for-byte the
//! pre-pullup algorithm.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn syllables(word: &str) -> usize {
        let w = word.to_ascii_lowercase();
        let letters: Vec<char> = w.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        if letters.is_empty() {
            return 0;
        }
        let vowel = |c: char| "aeiouy".contains(c);
        let mut count = 0usize;
        let mut prev = false;
        for &c in &letters {
            let v = vowel(c);
            if v && !prev {
                count += 1;
            }
            prev = v;
        }
        if letters.last() == Some(&'e') && count > 1 {
            count -= 1;
        }
        count.max(1)
    }

    /// Returns (word_count, sentence_count, syllable_count).
    pub fn counts(s: &str) -> (usize, usize, usize) {
        let words: Vec<&str> = s.split_whitespace().collect();
        let wc = words.len();
        let sc = s
            .chars()
            .filter(|c| *c == '.' || *c == '!' || *c == '?')
            .count()
            .max(if wc > 0 { 1 } else { 0 });
        let syl = words.iter().map(|w| syllables(w)).sum();
        (wc, sc, syl)
    }

    // Suppress an unused-import lint when String happens to be unreferenced.
    #[allow(dead_code)]
    fn _use_string(_: String) {}
}

datalink_extcore::declare! {
    core = Core;
    extension = "textstat";
    version = env!("CARGO_PKG_VERSION");

    scalar word_count(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "word_count")?;
        Ok(NeutralValue::Int64(logic::counts(&s).0 as i64))
    };

    scalar sentence_count(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "sentence_count")?;
        Ok(NeutralValue::Int64(logic::counts(&s).1 as i64))
    };

    scalar syllable_count(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "syllable_count")?;
        Ok(NeutralValue::Int64(logic::counts(&s).2 as i64))
    };

    scalar flesch_reading_ease(text) -> float64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "flesch_reading_ease")?;
        let (wc, sc, syl) = logic::counts(&s);
        if wc == 0 || sc == 0 {
            Ok(NeutralValue::Null)
        } else {
            let score = 206.835 - 1.015 * (wc as f64 / sc as f64) - 84.6 * (syl as f64 / wc as f64);
            Ok(NeutralValue::Float64(score))
        }
    };

    scalar reading_time_minutes(text) -> float64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "reading_time_minutes")?;
        Ok(NeutralValue::Float64(logic::counts(&s).0 as f64 / 200.0))
    };
}
