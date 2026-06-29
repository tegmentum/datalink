//! Neutral core for the `hocon` extension — HOCON (Typesafe Config) parsing
//! via the `hocon` crate — written ONCE. The per-DB shims are generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `hocon_to_json(text) -> text` — HOCON document -> JSON object string
//!     (substitutions resolved during parse). Invalid input -> NULL.
//!   * `hocon_get(text, path text) -> text` — value at a dotted path
//!     (e.g. `db.host`); NULL if the path is absent or the input is invalid.
//!
//! URL-include support is compiled out (no network from wasm). Never panics;
//! all error paths return NULL. Identical in both ports.

extern crate alloc;

use alloc::string::{String, ToString};
use datalink_extcore::{ArgExt, NeutralValue};
use hocon::{Hocon, HoconLoader};

/// Parse a HOCON document. URL-include support is compiled out (the crate's
/// `url-support` feature is disabled), so no network is reachable from wasm.
fn parse(text: &str) -> Option<Hocon> {
    HoconLoader::new().load_str(text).ok()?.hocon().ok()
}

/// Convert a parsed `Hocon` tree into a `serde_json::Value`. `BadValue`
/// nodes (parse/lookup errors) map to JSON null.
fn hocon_to_value(h: &Hocon) -> serde_json::Value {
    use serde_json::Value;
    match h {
        Hocon::Real(r) => serde_json::Number::from_f64(*r).map(Value::Number).unwrap_or(Value::Null),
        Hocon::Integer(i) => Value::Number((*i).into()),
        Hocon::String(s) => Value::String(s.clone()),
        Hocon::Boolean(b) => Value::Bool(*b),
        Hocon::Array(a) => Value::Array(a.iter().map(hocon_to_value).collect()),
        Hocon::Hash(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m.iter() {
                obj.insert(k.clone(), hocon_to_value(v));
            }
            Value::Object(obj)
        }
        Hocon::Null | Hocon::BadValue(_) => Value::Null,
    }
}

/// `hocon_to_json`: parse, then serialize the whole tree as JSON text.
pub fn to_json(text: &str) -> Option<String> {
    parse(text)
        .map(|h| hocon_to_value(&h))
        .and_then(|v| serde_json::to_string(&v).ok())
}

/// Walk a dotted path (e.g. "db.host"). Numeric segments index arrays.
/// Returns the leaf as a text string, or None if absent.
pub fn get_path(text: &str, path: &str) -> Option<String> {
    let root = parse(text)?;
    let mut cur = &root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        let next = match cur {
            Hocon::Array(_) => match seg.parse::<usize>() {
                Ok(i) => &cur[i],
                Err(_) => return None,
            },
            Hocon::Hash(_) => &cur[seg],
            _ => return None,
        };
        if let Hocon::BadValue(_) = next {
            return None;
        }
        cur = next;
    }
    match cur {
        Hocon::String(s) => Some(s.clone()),
        Hocon::Integer(i) => Some(i.to_string()),
        Hocon::Real(r) => Some(r.to_string()),
        Hocon::Boolean(b) => Some(b.to_string()),
        Hocon::Null => None,
        Hocon::Array(_) | Hocon::Hash(_) => serde_json::to_string(&hocon_to_value(cur)).ok(),
        Hocon::BadValue(_) => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "hocon";
    version = env!("CARGO_PKG_VERSION");

    scalar hocon_to_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "hocon_to_json")?;
        Ok(match to_json(&s) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };

    scalar hocon_get(text, text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "hocon_get")?;
        let p = args.arg_text(1, "hocon_get")?;
        Ok(match get_path(&s, &p) {
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
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }

    #[test]
    fn get_and_json() {
        let doc = "db { host = localhost, port = 5432 }";
        assert_eq!(
            Core::dispatch(idx("hocon_get"), &[t(doc), t("db.host")]).unwrap(),
            t("localhost")
        );
        assert_eq!(
            Core::dispatch(idx("hocon_get"), &[t(doc), t("db.port")]).unwrap(),
            t("5432")
        );
        // absent path -> NULL
        assert_eq!(
            Core::dispatch(idx("hocon_get"), &[t(doc), t("db.missing")]).unwrap(),
            NeutralValue::Null
        );
        // to_json produces a non-empty object string
        match Core::dispatch(idx("hocon_to_json"), &[t(doc)]).unwrap() {
            NeutralValue::Text(s) => assert!(s.contains("localhost")),
            other => panic!("{other:?}"),
        }
    }
}
