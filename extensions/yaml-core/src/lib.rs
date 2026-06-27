//! Neutral core for the `yaml` extension — YAML <-> JSON, bridged
//! through `serde_json::Value` — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `yaml_to_json(yaml) -> text`  (JSON)
//!   * `json_to_yaml(json) -> text`  (YAML)
//!
//! Invalid input -> NULL. The surface is identical in both ports (zero
//! drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub fn yaml_to_json(s: &str) -> Option<String> {
    serde_yaml::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
}

pub fn json_to_yaml(s: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_yaml::to_string(&v).ok())
}

fn opt_text(o: Option<String>) -> NeutralValue {
    match o {
        Some(s) => NeutralValue::Text(s),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "yaml";
    version = env!("CARGO_PKG_VERSION");

    scalar yaml_to_json(text) -> text [propagate, deterministic] = |args| {
        Ok(opt_text(yaml_to_json(&args.arg_text(0, "yaml_to_json")?)))
    };
    scalar json_to_yaml(text) -> text [propagate, deterministic] = |args| {
        Ok(opt_text(json_to_yaml(&args.arg_text(0, "json_to_yaml")?)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("yaml_to_json"), &[t("name: Alice\nage: 30")]).unwrap(), t(r#"{"age":30,"name":"Alice"}"#));
        assert_eq!(Core::dispatch(idx("yaml_to_json"), &[t("[1, 2, 3]")]).unwrap(), t("[1,2,3]"));
        assert_eq!(Core::dispatch(idx("yaml_to_json"), &[t(": : invalid : :")]).unwrap(), NeutralValue::Null);
    }
}
