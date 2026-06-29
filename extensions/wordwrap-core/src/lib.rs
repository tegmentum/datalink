//! Neutral core for the `wordwrap` extension — greedy word-wrap (via the
//! `textwrap` crate) — written ONCE. The per-DB shims (ducklink
//! `duckdb:extension`, sqlink `sqlite:extension`, sqlink embed) are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `word_wrap(text, width) -> text` — greedy fill to `width` columns,
//!     newline separated. NULL text -> NULL; non-positive width -> NULL.

extern crate alloc;

use datalink_extcore::{ArgExt, NeutralValue};

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    /// Greedy fill to `width` columns. `None` if `width <= 0`.
    pub fn word_wrap(text: &str, width: i64) -> Option<String> {
        if width <= 0 {
            return None;
        }
        Some(textwrap::fill(text, width as usize))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "wordwrap";
    version = env!("CARGO_PKG_VERSION");

    scalar word_wrap(text, int64) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "word_wrap")?;
        let width = args.arg_int(1, "word_wrap")?;
        Ok(match logic::word_wrap(&s, width) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn wraps_to_width() {
        let out = Core::dispatch(
            idx("word_wrap"),
            &[NeutralValue::Text(String::from("the quick brown fox")), NeutralValue::Int64(9)],
        )
        .unwrap();
        assert_eq!(out, NeutralValue::Text(String::from("the quick\nbrown fox")));
    }

    #[test]
    fn non_positive_width_is_null() {
        let out = Core::dispatch(
            idx("word_wrap"),
            &[NeutralValue::Text(String::from("hello")), NeutralValue::Int64(0)],
        )
        .unwrap();
        assert_eq!(out, NeutralValue::Null);
    }
}
