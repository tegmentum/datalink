//! Neutral core for the `dotenv` extension — hand-rolled `.env` parsing —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `dotenv_to_json(text)    -> text`  JSON object {KEY: VALUE} (file order)
//!   * `dotenv_get(text, key)   -> text`  value for KEY (last wins); NULL if absent
//!   * `dotenv_keys(text)       -> text`  JSON array of keys (file order, deduped)
//!
//! KEY=VALUE lines, `#` comments, blank lines, optional `export ` prefix, and
//! matched single/double quotes stripped. NULL input / missing values -> NULL,
//! byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): `serde_json` is built with std here; `extern crate
//! alloc` keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup parser (DB-agnostic).
pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use serde_json::Value;

    /// Parse one .env line into (key, value); None for blank/comment/`=`-less.
    pub fn parse_line(line: &str) -> Option<(String, String)> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let eq = line.find('=')?;
        let key = line[..eq].trim();
        if key.is_empty() {
            return None;
        }
        let raw = line[eq + 1..].trim_start();
        Some((key.to_string(), parse_value(raw)))
    }

    /// Resolve the RHS: strip matched surrounding quotes, else drop an inline
    /// `# comment` and trim trailing whitespace.
    pub fn parse_value(raw: &str) -> String {
        let bytes = raw.as_bytes();
        if bytes.len() >= 2 {
            let q = bytes[0];
            if (q == b'"' || q == b'\'') && bytes[bytes.len() - 1] == q {
                return raw[1..raw.len() - 1].to_string();
            }
        }
        let mut end = raw.len();
        let b = raw.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
                end = i;
                break;
            }
            i += 1;
        }
        raw[..end].trim_end().to_string()
    }

    pub fn entries(src: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in src.lines() {
            if let Some(kv) = parse_line(line) {
                out.push(kv);
            }
        }
        out
    }

    pub fn to_json(src: &str) -> Option<String> {
        let mut obj = serde_json::Map::new();
        for (k, v) in entries(src) {
            obj.insert(k, Value::String(v));
        }
        serde_json::to_string(&Value::Object(obj)).ok()
    }

    pub fn get(src: &str, key: &str) -> Option<String> {
        entries(src).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn keys(src: &str) -> Option<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut arr: Vec<Value> = Vec::new();
        for (k, _) in entries(src) {
            if !seen.contains(&k) {
                seen.push(k.clone());
                arr.push(Value::String(k));
            }
        }
        serde_json::to_string(&Value::Array(arr)).ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "dotenv";
    version = env!("CARGO_PKG_VERSION");

    scalar dotenv_to_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "dotenv_to_json")?;
        Ok(match logic::to_json(&s) {
            Some(j) => NeutralValue::Text(j),
            None => NeutralValue::Null,
        })
    };

    scalar dotenv_get(text, text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "dotenv_get")?;
        let k = args.arg_text(1, "dotenv_get")?;
        Ok(match logic::get(&s, &k) {
            Some(v) => NeutralValue::Text(v),
            None => NeutralValue::Null,
        })
    };

    scalar dotenv_keys(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "dotenv_keys")?;
        Ok(match logic::keys(&s) {
            Some(j) => NeutralValue::Text(j),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;

    const ENV: &str = "# c\nHOST=localhost\nPORT=\"5432\"";

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert_eq!(
            Core::dispatch(idx("dotenv_get"), &[t(ENV), t("HOST")]).unwrap(),
            NeutralValue::Text(String::from("localhost"))
        );
        assert_eq!(
            Core::dispatch(idx("dotenv_get"), &[t(ENV), t("PORT")]).unwrap(),
            NeutralValue::Text(String::from("5432"))
        );
        assert_eq!(
            Core::dispatch(idx("dotenv_keys"), &[t(ENV)]).unwrap(),
            NeutralValue::Text(String::from("[\"HOST\",\"PORT\"]"))
        );
        assert_eq!(
            Core::dispatch(idx("dotenv_get"), &[t(ENV), t("MISSING")]).unwrap(),
            NeutralValue::Null
        );
    }
}
