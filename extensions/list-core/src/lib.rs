//! Neutral core for the `list` extension — JSON-array-backed list
//! scalars, written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the names DuckDB does NOT ship as builtins
//!
//! `list` originated in sqlink because SQLite has no array/list type at
//! all. DuckDB is the opposite: it has a native `LIST` type and ships a
//! rich `list_*`/`array_*` family as BUILTINS —
//! `array_append`/`array_prepend`/`array_cat`/`array_concat`/
//! `array_length`/`array_position`/`array_to_string`/`array_slice`/
//! `array_sort`/`array_distinct`/`array_contains`/`array_reverse`/
//! `flatten`/`len`/the `list_*` mirrors/`list_min`/`list_max`/`list_sum`/
//! `list_product`/`list_avg`/`list_count`/`array_intersect`/
//! `list_intersect`/`array_to_json`. Re-registering any of those (same
//! name + arity) would collide with the builtin, so they are deliberately
//! NOT declared here.
//!
//! What remains — and is what ducklink GAINS — are the PostgreSQL-flavour
//! names DuckDB has no builtin for, operating on the same TEXT/JSON-array
//! carrier as sqlink: `array_remove`, `list_length`, the `array_*`
//! reductions, `array_dims`/`array_lower`/`array_upper`/`array_ndims`,
//! `array_positions`, `array_replace`, `arrays_overlap`. The carrier and
//! algorithms are byte-identical to sqlink's `list`, so a future
//! `sqlite_shim!` over this core reproduces sqlink's behaviour.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic list algorithms, lifted byte-for-byte from sqlink's `list`
/// extension. Native-testable; the generated shim is a thin wrapper.
pub mod algo {
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use serde_json::Value;

    pub fn parse_array(s: &str) -> Result<Vec<Value>, String> {
        match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(arr)) => Ok(arr),
            Ok(_) => Err("array argument is not a JSON array".to_string()),
            Err(e) => Err(format!("array parse: {e}")),
        }
    }

    /// Accept either a JSON-encoded value OR a bare TEXT (fall back to
    /// String). Lets callers write `array_remove(a, 'foo')` AND
    /// `array_remove(a, '"foo"')` AND `array_remove(a, '42')`.
    pub fn parse_value(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or(Value::String(s.to_string()))
    }

    pub fn to_json(arr: &[Value]) -> String {
        serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string())
    }

    pub fn remove(arr: Vec<Value>, needle: &Value) -> Vec<Value> {
        arr.into_iter().filter(|x| x != needle).collect()
    }

    fn to_num(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn list_min(arr: &[Value]) -> Option<f64> {
        arr.iter().filter_map(to_num).fold(None, |a, x| {
            Some(match a {
                Some(v) => v.min(x),
                None => x,
            })
        })
    }
    pub fn list_max(arr: &[Value]) -> Option<f64> {
        arr.iter().filter_map(to_num).fold(None, |a, x| {
            Some(match a {
                Some(v) => v.max(x),
                None => x,
            })
        })
    }
    pub fn list_sum(arr: &[Value]) -> f64 {
        arr.iter().filter_map(to_num).sum()
    }
    pub fn list_product(arr: &[Value]) -> f64 {
        arr.iter().filter_map(to_num).fold(1.0, |a, x| a * x)
    }
    pub fn list_avg(arr: &[Value]) -> Option<f64> {
        let mut n = 0usize;
        let mut s = 0.0;
        for x in arr.iter().filter_map(to_num) {
            n += 1;
            s += x;
        }
        if n == 0 {
            None
        } else {
            Some(s / n as f64)
        }
    }
    /// Count non-null elements (DuckDB semantics).
    pub fn list_count(arr: &[Value]) -> i64 {
        arr.iter().filter(|v| !matches!(v, Value::Null)).count() as i64
    }
}

/// Convert a neutral argument to a `serde_json::Value`, mirroring
/// sqlink's `as_json_value` (Text -> parse_value; Int/Float -> Number;
/// Bool -> Bool; Blob -> String; Null -> Null).
fn as_json_value(v: &NeutralValue) -> serde_json::Value {
    use serde_json::Value;
    match v {
        NeutralValue::Null => Value::Null,
        NeutralValue::Boolean(b) => Value::Bool(*b),
        NeutralValue::Int64(n) => Value::from(*n),
        NeutralValue::Float64(r) => serde_json::Number::from_f64(*r)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        NeutralValue::Text(s) => algo::parse_value(s),
        NeutralValue::Blob(b) => Value::String(alloc::string::String::from_utf8_lossy(b).into_owned()),
        NeutralValue::Complex { json, .. } => algo::parse_value(json),
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "list";
    version = env!("CARGO_PKG_VERSION");

    // PostgreSQL `array_remove(arr, v)` — DuckDB has list_filter, no
    // array_remove. The value is taken as TEXT (DuckDB casts the literal)
    // then JSON-decoded, matching sqlink's as_json_value.
    scalar array_remove(text, text) -> text [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_remove")?)?;
        let v = as_json_value(a.get(1).unwrap_or(&NeutralValue::Null));
        Ok(NeutralValue::Text(algo::to_json(&algo::remove(arr, &v))))
    };

    // `list_length(arr)` — DuckDB has len/array_length, not list_length.
    scalar list_length(text) -> int64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "list_length")?)?;
        Ok(NeutralValue::Int64(arr.len() as i64))
    };

    // array_* numeric reductions over a JSON array (DuckDB has list_min/…
    // but not the array_* spellings). NULL on empty/no-numeric.
    scalar array_min(text) -> float64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_min")?)?;
        Ok(algo::list_min(&arr).map(NeutralValue::Float64).unwrap_or(NeutralValue::Null))
    };
    scalar array_max(text) -> float64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_max")?)?;
        Ok(algo::list_max(&arr).map(NeutralValue::Float64).unwrap_or(NeutralValue::Null))
    };
    scalar array_sum(text) -> float64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_sum")?)?;
        Ok(NeutralValue::Float64(algo::list_sum(&arr)))
    };
    scalar array_product(text) -> float64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_product")?)?;
        Ok(NeutralValue::Float64(algo::list_product(&arr)))
    };
    scalar array_avg(text) -> float64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_avg")?)?;
        Ok(algo::list_avg(&arr).map(NeutralValue::Float64).unwrap_or(NeutralValue::Null))
    };
    scalar array_count(text) -> int64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_count")?)?;
        Ok(NeutralValue::Int64(algo::list_count(&arr)))
    };

    // PG dimension introspection on a 1-D JSON array. NULL on empty.
    scalar array_dims(text) -> text [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_dims")?)?;
        if arr.is_empty() {
            return Ok(NeutralValue::Null);
        }
        Ok(NeutralValue::Text(::alloc::format!("[1:{}]", arr.len())))
    };
    scalar array_lower(text) -> int64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_lower")?)?;
        Ok(if arr.is_empty() { NeutralValue::Null } else { NeutralValue::Int64(1) })
    };
    scalar array_upper(text) -> int64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_upper")?)?;
        Ok(if arr.is_empty() { NeutralValue::Null } else { NeutralValue::Int64(arr.len() as i64) })
    };
    scalar array_ndims(text) -> int64 [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_ndims")?)?;
        Ok(if arr.is_empty() { NeutralValue::Null } else { NeutralValue::Int64(1) })
    };

    // PG `array_positions(arr, v)` — all 1-based match indices as a JSON
    // array.
    scalar array_positions(text, text) -> text [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_positions")?)?;
        let v = as_json_value(a.get(1).unwrap_or(&NeutralValue::Null));
        let hits: ::alloc::vec::Vec<serde_json::Value> = arr
            .iter()
            .enumerate()
            .filter(|(_, x)| **x == v)
            .map(|(i, _)| serde_json::Value::from((i + 1) as i64))
            .collect();
        Ok(NeutralValue::Text(serde_json::to_string(&hits).unwrap_or_default()))
    };

    // PG `array_replace(arr, from, to)`.
    scalar array_replace(text, text, text) -> text [propagate, deterministic] = |a| {
        let arr = algo::parse_array(&a.arg_text(0, "array_replace")?)?;
        let from = as_json_value(a.get(1).unwrap_or(&NeutralValue::Null));
        let to = as_json_value(a.get(2).unwrap_or(&NeutralValue::Null));
        let out: ::alloc::vec::Vec<serde_json::Value> = arr
            .into_iter()
            .map(|v| if v == from { to.clone() } else { v })
            .collect();
        Ok(NeutralValue::Text(algo::to_json(&out)))
    };

    // PG `arrays_overlap(a, b)` -> 0/1 (DuckDB has list_has_any, not this
    // spelling). Returns INTEGER 0/1, matching sqlink.
    scalar arrays_overlap(text, text) -> int64 [propagate, deterministic] = |a| {
        let x = algo::parse_array(&a.arg_text(0, "arrays_overlap")?)?;
        let y = algo::parse_array(&a.arg_text(1, "arrays_overlap")?)?;
        let overlap = x.iter().any(|p| y.iter().any(|q| p == q));
        Ok(NeutralValue::Int64(overlap as i64))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(n: &str, arity: usize) -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == n && d.args.len() == arity)
            .unwrap_or_else(|| panic!("no decl {n}/{arity}"))
    }
    fn call(n: &str, arity: usize, args: &[NeutralValue]) -> NeutralValue {
        Core::dispatch(idx(n, arity), args).unwrap()
    }

    #[test]
    fn remove_and_length() {
        assert_eq!(
            call(
                "array_remove",
                2,
                &[NeutralValue::Text("[1,2,3,2,4,2]".into()), NeutralValue::Int64(2)]
            ),
            NeutralValue::Text("[1,3,4]".into())
        );
        assert_eq!(
            call("list_length", 1, &[NeutralValue::Text("[1,2,3,4,5]".into())]),
            NeutralValue::Int64(5)
        );
    }

    #[test]
    fn reductions() {
        assert_eq!(
            call("array_sum", 1, &[NeutralValue::Text("[1,2,3,4]".into())]),
            NeutralValue::Float64(10.0)
        );
        assert_eq!(
            call("array_max", 1, &[NeutralValue::Text("[3,1,4,1,5]".into())]),
            NeutralValue::Float64(5.0)
        );
        assert_eq!(
            call("array_count", 1, &[NeutralValue::Text("[1,null,3]".into())]),
            NeutralValue::Int64(2)
        );
        assert_eq!(
            call("array_min", 1, &[NeutralValue::Text("[]".into())]),
            NeutralValue::Null
        );
    }

    #[test]
    fn dims_and_overlap() {
        assert_eq!(
            call("array_upper", 1, &[NeutralValue::Text("[10,20,30]".into())]),
            NeutralValue::Int64(3)
        );
        assert_eq!(
            call("array_dims", 1, &[NeutralValue::Text("[1,2]".into())]),
            NeutralValue::Text("[1:2]".into())
        );
        assert_eq!(
            call(
                "arrays_overlap",
                2,
                &[NeutralValue::Text("[1,2,3]".into()), NeutralValue::Text("[3,4,5]".into())]
            ),
            NeutralValue::Int64(1)
        );
        assert_eq!(
            call(
                "array_positions",
                2,
                &[NeutralValue::Text("[5,6,5,7,5]".into()), NeutralValue::Int64(5)]
            ),
            NeutralValue::Text("[1,3,5]".into())
        );
        assert_eq!(
            call(
                "array_replace",
                3,
                &[
                    NeutralValue::Text("[1,2,3,2]".into()),
                    NeutralValue::Int64(2),
                    NeutralValue::Int64(9)
                ]
            ),
            NeutralValue::Text("[1,9,3,9]".into())
        );
    }
}
