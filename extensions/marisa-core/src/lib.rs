//! Neutral core for the `marisa` extension — trie-backed string set
//! lookup / prefix search via the `fst` crate — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `fst_contains(terms_json, key) -> boolean`
//!   * `fst_prefix(terms_json, prefix) -> text` (JSON array, sorted)
//!   * `fst_count(terms_json) -> int64`
//!
//! NULL / invalid input -> NULL. Never panics.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use fst::Set;

    /// Parse a JSON array of strings, sort + dedup (lexicographic order
    /// required by fst), build an in-memory fst Set. None on bad input.
    pub fn build_set(terms_json: &str) -> Option<(Set<Vec<u8>>, Vec<String>)> {
        let parsed: serde_json::Value = serde_json::from_str(terms_json).ok()?;
        let arr = parsed.as_array()?;
        let mut terms: Vec<String> = Vec::with_capacity(arr.len());
        for v in arr {
            terms.push(v.as_str()?.to_string());
        }
        terms.sort();
        terms.dedup();
        let set = Set::from_iter(terms.iter()).ok()?;
        Some((set, terms))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "marisa";
    version = env!("CARGO_PKG_VERSION");

    scalar fst_contains(text, text) -> boolean [propagate, deterministic] = |args| {
        let terms = args.arg_text(0, "fst_contains")?;
        let key = args.arg_text(1, "fst_contains")?;
        Ok(match logic::build_set(&terms) {
            Some((set, _)) => NeutralValue::Boolean(set.contains(key.as_str())),
            None => NeutralValue::Null,
        })
    };

    scalar fst_prefix(text, text) -> text [propagate, deterministic] = |args| {
        use fst::{Automaton, IntoStreamer, Streamer};
        use fst::automaton::Str;
        let terms = args.arg_text(0, "fst_prefix")?;
        let prefix = args.arg_text(1, "fst_prefix")?;
        Ok(match logic::build_set(&terms) {
            Some((set, _)) => {
                let mut matched: Vec<String> = Vec::new();
                let mut stream =
                    set.search(Str::new(prefix.as_str()).starts_with()).into_stream();
                while let Some(k) = stream.next() {
                    if let Ok(s) = core::str::from_utf8(k) {
                        matched.push(s.to_string());
                    }
                }
                match serde_json::to_string(&matched) {
                    Ok(j) => NeutralValue::Text(j),
                    Err(_) => NeutralValue::Null,
                }
            }
            None => NeutralValue::Null,
        })
    };

    scalar fst_count(text) -> int64 [propagate, deterministic] = |args| {
        let terms = args.arg_text(0, "fst_count")?;
        Ok(match logic::build_set(&terms) {
            Some((set, _)) => NeutralValue::Int64(set.len() as i64),
            None => NeutralValue::Null,
        })
    };
}
