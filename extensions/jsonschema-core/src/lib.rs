//! Neutral core for the `jsonschema` extension — JSON Schema validation
//! via the `jsonschema` crate — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `json_schema_valid(schema, instance) -> boolean` — both args are
//!     JSON text. Malformed JSON or an invalid schema -> NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "jsonschema";
    version = env!("CARGO_PKG_VERSION");

    scalar json_schema_valid(text, text) -> boolean [propagate, deterministic] = |args| {
        let schema_s = args.arg_text(0, "json_schema_valid")?;
        let instance_s = args.arg_text(1, "json_schema_valid")?;
        let schema: serde_json::Value = match serde_json::from_str(&schema_s) {
            Ok(v) => v,
            Err(_) => return Ok(NeutralValue::Null),
        };
        let instance: serde_json::Value = match serde_json::from_str(&instance_s) {
            Ok(v) => v,
            Err(_) => return Ok(NeutralValue::Null),
        };
        Ok(match jsonschema::validator_for(&schema) {
            Ok(v) => NeutralValue::Boolean(v.is_valid(&instance)),
            Err(_) => NeutralValue::Null,
        })
    };
}
