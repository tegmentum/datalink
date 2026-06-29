//! Neutral core for the `ion` extension — Amazon Ion <-> JSON conversion (via
//! `ion-rs`) — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ion_to_json(text) -> text`        Ion text -> JSON text
//!   * `ion_from_json(text) -> text`      JSON text -> Ion text
//!   * `ion_get(text, field) -> text`     top-level Ion struct field as text
//!
//! NULL input -> NULL; parse error -> NULL; never panics.

extern crate alloc;

use alloc::string::{String, ToString};
use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ion_rs::{Element, IonType, Value};
    use serde_json::Value as Json;

    /// Render an Ion `Element` as a `serde_json::Value`. Ion types with no JSON
    /// analogue fall back to their Ion text rendering as a JSON string.
    pub fn element_to_json(e: &Element) -> Json {
        if e.is_null() {
            return Json::Null;
        }
        match e.value() {
            Value::Null(_) => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(_) => match e.as_i64() {
                Some(n) => Json::from(n),
                None => Json::String(e.to_string()),
            },
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Value::Decimal(_) => match e.as_float() {
                Some(f) => serde_json::Number::from_f64(f).map(Json::Number).unwrap_or_else(|| Json::String(e.to_string())),
                None => Json::String(e.to_string()),
            },
            Value::String(s) => Json::String(s.text().to_string()),
            Value::Symbol(s) => match s.text() {
                Some(t) => Json::String(t.to_string()),
                None => Json::Null,
            },
            Value::List(seq) | Value::SExp(seq) => {
                Json::Array(seq.into_iter().map(element_to_json).collect())
            }
            Value::Struct(st) => {
                let mut map = serde_json::Map::new();
                for (name, val) in st {
                    let key = name.text().unwrap_or("$0").to_string();
                    map.insert(key, element_to_json(val));
                }
                Json::Object(map)
            }
            _ => Json::String(e.to_string()),
        }
    }

    /// Build an Ion `Element` from a `serde_json::Value`.
    pub fn json_to_element(j: &Json) -> Element {
        match j {
            Json::Null => Element::null(IonType::Null),
            Json::Bool(b) => Element::from(*b),
            Json::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Element::from(i)
                } else if let Some(u) = n.as_u64() {
                    Element::from(u as i64)
                } else {
                    Element::from(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Json::String(s) => Element::from(s.as_str()),
            Json::Array(arr) => {
                let items: Vec<Element> = arr.iter().map(json_to_element).collect();
                Element::from(Value::List(ion_rs::Sequence::new(items)))
            }
            Json::Object(map) => {
                let mut b = ion_rs::Struct::builder();
                for (k, v) in map {
                    b = b.with_field(k.as_str(), json_to_element(v));
                }
                Element::from(b.build())
            }
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ion";
    version = env!("CARGO_PKG_VERSION");

    scalar ion_to_json(text) -> text [propagate, deterministic] = |args| {
        use ion_rs::Element;
        let s = args.arg_text(0, "ion_to_json")?;
        Ok(match Element::read_one(s.as_bytes()) {
            Ok(el) => match serde_json::to_string(&logic::element_to_json(&el)) {
                Ok(out) => NeutralValue::Text(out),
                Err(_) => NeutralValue::Null,
            },
            Err(_) => NeutralValue::Null,
        })
    };
    scalar ion_from_json(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ion_from_json")?;
        Ok(match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(j) => NeutralValue::Text(logic::json_to_element(&j).to_string()),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar ion_get(text, text) -> text [propagate, deterministic] = |args| {
        use ion_rs::Element;
        let s = args.arg_text(0, "ion_get")?;
        let field = args.arg_text(1, "ion_get")?;
        Ok(match Element::read_one(s.as_bytes()) {
            Ok(el) => match el.as_struct().and_then(|st| st.get(field.as_str())) {
                Some(v) => NeutralValue::Text(v.to_string()),
                None => NeutralValue::Null,
            },
            Err(_) => NeutralValue::Null,
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
        assert_eq!(Core::dispatch(idx("ion_to_json"), &[t("{a:1, b:\"hi\"}")]).unwrap(), t("{\"a\":1,\"b\":\"hi\"}"));
        assert_eq!(Core::dispatch(idx("ion_get"), &[t("{a:1, b:2}"), t("b")]).unwrap(), t("2"));
        assert_eq!(Core::dispatch(idx("ion_to_json"), &[t("{not valid")]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("ion_from_json"), &[t("{not json")]).unwrap(), NeutralValue::Null);
    }
}
