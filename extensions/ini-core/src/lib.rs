//! Neutral core for the `ini` extension — INI / config-file parsing (via
//! `rust-ini`, bridged through serde_json) — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ini_to_json(text) -> text`                 {section: {key: value}}
//!   * `ini_get(text, section, key) -> text`       value, NULL if absent
//!   * `ini_sections(text) -> text`                JSON array of section names
//!
//! Keys outside any [section] go under the "" key. Invalid input / missing
//! value -> NULL. Never panics.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use serde_json::{Map, Value};

    /// Parse INI into a serde_json object-of-objects. General (no-section) keys
    /// go under "". Returns None on parse error.
    pub fn parse(src: &str) -> Option<Value> {
        let conf = ini::Ini::load_from_str(src).ok()?;
        let mut root = Map::new();
        for (sec, props) in conf.iter() {
            if sec.is_none() && props.is_empty() { continue; }
            let name = sec.unwrap_or("").to_string();
            let entry = root.entry(name).or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = entry {
                for (k, v) in props.iter() {
                    m.insert(k.to_string(), Value::String(v.to_string()));
                }
            }
        }
        Some(Value::Object(root))
    }

    pub fn ini_to_json(src: &str) -> Option<String> {
        serde_json::to_string(&parse(src)?).ok()
    }

    pub fn ini_get(src: &str, section: &str, key: &str) -> Option<String> {
        let conf = ini::Ini::load_from_str(src).ok()?;
        let sec = if section.is_empty() { None } else { Some(section) };
        conf.get_from(sec, key).map(|s| s.to_string())
    }

    pub fn ini_sections(src: &str) -> Option<String> {
        let conf = ini::Ini::load_from_str(src).ok()?;
        let mut seen: Vec<String> = Vec::new();
        let mut names: Vec<Value> = Vec::new();
        for (sec, props) in conf.iter() {
            if sec.is_none() && props.is_empty() { continue; }
            let name = sec.unwrap_or("").to_string();
            if !seen.contains(&name) {
                seen.push(name.clone());
                names.push(Value::String(name));
            }
        }
        serde_json::to_string(&Value::Array(names)).ok()
    }
}

fn opt(v: Option<String>) -> NeutralValue {
    match v { Some(t) => NeutralValue::Text(t), None => NeutralValue::Null }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ini";
    version = env!("CARGO_PKG_VERSION");

    scalar ini_to_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ini_to_json")?;
        Ok(opt(logic::ini_to_json(&s)))
    };
    scalar ini_get(text, text, text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ini_get")?;
        let sec = args.arg_text(1, "ini_get")?;
        let key = args.arg_text(2, "ini_get")?;
        Ok(opt(logic::ini_get(&s, &sec, &key)))
    };
    scalar ini_sections(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ini_sections")?;
        Ok(opt(logic::ini_sections(&s)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        let ini = "[db]\nhost=localhost\nport=5432";
        assert_eq!(Core::dispatch(idx("ini_get"), &[t(ini), t("db"), t("host")]).unwrap(), t("localhost"));
        assert_eq!(Core::dispatch(idx("ini_sections"), &[t(ini)]).unwrap(), t("[\"db\"]"));
        assert_eq!(Core::dispatch(idx("ini_to_json"), &[t(ini)]).unwrap(), t("{\"db\":{\"host\":\"localhost\",\"port\":\"5432\"}}"));
        assert_eq!(Core::dispatch(idx("ini_get"), &[t("[db]\nhost=localhost"), t("db"), t("missing")]).unwrap(), NeutralValue::Null);
    }
}
