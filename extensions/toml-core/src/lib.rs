//! Neutral core for the `toml` extension — TOML <-> JSON, bridged
//! through `serde_json::Value` — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `toml_to_json(toml) -> text`  (JSON)
//!   * `json_to_toml(json) -> text`  (TOML; requires a top-level object)
//!
//! Invalid input -> NULL. The surface is identical in both ports (zero
//! drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub fn toml_to_json(s: &str) -> Option<String> {
    toml::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
}

pub fn json_to_toml(s: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| toml::to_string(&v).ok())
}

fn opt_text(o: Option<String>) -> NeutralValue {
    match o {
        Some(s) => NeutralValue::Text(s),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "toml";
    version = env!("CARGO_PKG_VERSION");

    scalar toml_to_json(text) -> text [propagate, deterministic] = |args| {
        Ok(opt_text(toml_to_json(&args.arg_text(0, "toml_to_json")?)))
    };
    scalar json_to_toml(text) -> text [propagate, deterministic] = |args| {
        Ok(opt_text(json_to_toml(&args.arg_text(0, "json_to_toml")?)))
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
        assert_eq!(Core::dispatch(idx("toml_to_json"), &[t("title = \"x\"\ncount = 3")]).unwrap(), t(r#"{"count":3,"title":"x"}"#));
        assert_eq!(Core::dispatch(idx("json_to_toml"), &[t(r#"{"a":1,"b":[2,3]}"#)]).unwrap(), t("a = 1\nb = [2, 3]\n"));
        assert_eq!(Core::dispatch(idx("toml_to_json"), &[t("not valid = = toml")]).unwrap(), NeutralValue::Null);
    }
}
