//! Neutral core for the `json_schema` extension — JSON Schema validation (via
//! the `jsonschema` crate) — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `json_schema_valid(schema, doc) -> boolean`
//!   * `json_schema_errors(schema, doc) -> text`  (JSON array of error messages)
//!
//! NULL / unparseable input -> NULL. Never panics.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub mod logic {
    use serde_json::Value;

    /// Parse both args as JSON. None if either is not valid JSON.
    pub fn parse_pair(schema_txt: &str, doc_txt: &str) -> Option<(Value, Value)> {
        let schema: Value = serde_json::from_str(schema_txt).ok()?;
        let doc: Value = serde_json::from_str(doc_txt).ok()?;
        Some((schema, doc))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "json_schema";
    version = env!("CARGO_PKG_VERSION");

    scalar json_schema_valid(text, text) -> boolean [propagate, deterministic] = |args| {
        let schema_txt = args.arg_text(0, "json_schema_valid")?;
        let doc_txt = args.arg_text(1, "json_schema_valid")?;
        Ok(match logic::parse_pair(&schema_txt, &doc_txt) {
            Some((schema, doc)) => match jsonschema::validator_for(&schema) {
                Ok(v) => NeutralValue::Boolean(v.is_valid(&doc)),
                Err(_) => NeutralValue::Null,
            },
            None => NeutralValue::Null,
        })
    };
    scalar json_schema_errors(text, text) -> text [propagate, deterministic] = |args| {
        use alloc::string::ToString;
        use alloc::vec::Vec;
        let schema_txt = args.arg_text(0, "json_schema_errors")?;
        let doc_txt = args.arg_text(1, "json_schema_errors")?;
        Ok(match logic::parse_pair(&schema_txt, &doc_txt) {
            Some((schema, doc)) => match jsonschema::validator_for(&schema) {
                Ok(v) => {
                    let msgs: Vec<serde_json::Value> =
                        v.iter_errors(&doc).map(|e| serde_json::Value::String(e.to_string())).collect();
                    NeutralValue::Text(serde_json::Value::Array(msgs).to_string())
                }
                Err(_) => NeutralValue::Null,
            },
            None => NeutralValue::Null,
        })
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
        let schema = "{\"type\":\"object\",\"properties\":{\"age\":{\"type\":\"integer\"}},\"required\":[\"age\"]}";
        assert_eq!(Core::dispatch(idx("json_schema_valid"), &[t(schema), t("{\"age\":30}")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("json_schema_valid"), &[t(schema), t("{\"age\":\"old\"}")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("json_schema_errors"), &[t("{\"type\":\"integer\"}"), t("5")]).unwrap(), t("[]"));
    }
}
