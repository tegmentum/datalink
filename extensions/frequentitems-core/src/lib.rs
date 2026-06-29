//! Neutral core for the `frequentitems` extension — top-K / heavy-hitters
//! over a JSON array — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `top_k(values_json text, k bigint) -> text` — JSON array of
//!     `{"value":..,"count":..}` for the K most frequent values.
//!   * `top_k_value(values_json text, k bigint) -> text` — JSON array of just
//!     the K most frequent values.
//!
//! Exact counts via a hashmap + sort; ties are broken by first-seen order so
//! the output is fully deterministic. NULL / bad input / k <= 0 -> NULL.
//! Never panics. Identical in both ports.
//!
//! NOTE: depends on `std` (not `#![no_std]`) for `std::collections::HashMap`.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use datalink_extcore::{ArgExt, NeutralValue};
use std::collections::HashMap;

/// Turn a JSON value into the string we count. Strings are used as-is;
/// numbers and bools use their JSON text; nulls are skipped (return None).
fn elem_to_key(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// Count occurrences and return the top-K (value, count) pairs. Ordered by
/// count desc, then by first-seen order for deterministic tie-breaks.
pub fn top_k_pairs(values_json: &str, k: i64) -> Option<Vec<(String, u64)>> {
    if k <= 0 {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(values_json).ok()?;
    let arr = parsed.as_array()?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut order: HashMap<String, usize> = HashMap::new();
    let mut next_seen: usize = 0;
    for elem in arr {
        if let Some(key) = elem_to_key(elem) {
            *counts.entry(key.clone()).or_insert(0) += 1;
            order.entry(key).or_insert_with(|| {
                let s = next_seen;
                next_seen += 1;
                s
            });
        }
    }
    let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| order[&a.0].cmp(&order[&b.0])));
    pairs.truncate(k as usize);
    Some(pairs)
}

datalink_extcore::declare! {
    core = Core;
    extension = "frequentitems";
    version = env!("CARGO_PKG_VERSION");

    scalar top_k(text, int64) -> text [propagate, deterministic] = |args| {
        let json = args.arg_text(0, "top_k")?;
        let k = args.arg_int(1, "top_k")?;
        Ok(match top_k_pairs(&json, k) {
            Some(pairs) => {
                let arr: Vec<serde_json::Value> = pairs
                    .into_iter()
                    .map(|(v, c)| serde_json::json!({ "value": v, "count": c }))
                    .collect();
                NeutralValue::Text(serde_json::Value::Array(arr).to_string())
            }
            None => NeutralValue::Null,
        })
    };

    scalar top_k_value(text, int64) -> text [propagate, deterministic] = |args| {
        let json = args.arg_text(0, "top_k_value")?;
        let k = args.arg_int(1, "top_k_value")?;
        Ok(match top_k_pairs(&json, k) {
            Some(pairs) => {
                let arr: Vec<serde_json::Value> = pairs
                    .into_iter()
                    .map(|(v, _)| serde_json::Value::String(v))
                    .collect();
                NeutralValue::Text(serde_json::Value::Array(arr).to_string())
            }
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }

    #[test]
    fn top_k_values_and_pairs() {
        let arr = r#"["a","b","a","c","a","b"]"#;
        assert_eq!(
            Core::dispatch(idx("top_k_value"), &[t(arr), NeutralValue::Int64(2)]).unwrap(),
            t(r#"["a","b"]"#)
        );
        assert_eq!(
            Core::dispatch(idx("top_k"), &[t(arr), NeutralValue::Int64(2)]).unwrap(),
            t(r#"[{"count":3,"value":"a"},{"count":2,"value":"b"}]"#)
        );
        // k <= 0 -> NULL
        assert_eq!(
            Core::dispatch(idx("top_k"), &[t(arr), NeutralValue::Int64(0)]).unwrap(),
            NeutralValue::Null
        );
    }
}
