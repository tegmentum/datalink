//! Neutral core for the `vcard` extension — vCard (.vcf) contact parsing
//! via the `ical` crate — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `vcard_count(vcf) -> int64`  (number of VCARD contacts)
//!   * `vcard_to_json(vcf) -> text` ([{fn,email,tel,org}, ...])
//!   * `vcard_names(vcf) -> text`   (["formatted name", ...])
//!
//! Parse error / non-text input -> NULL (never panics). The surface is
//! identical in both ports (zero drift).

extern crate alloc;

use datalink_extcore::NeutralValue;
use ical::VcardParser;

/// Parse every VCARD in `vcf`, returning all contacts.
/// Any parse error -> None (whole call yields NULL).
fn parse_contacts(vcf: &str) -> Option<Vec<ical::parser::vcard::component::VcardContact>> {
    let mut contacts = Vec::new();
    for c in VcardParser::new(vcf.as_bytes()) {
        contacts.push(c.ok()?);
    }
    Some(contacts)
}

/// First value of a named property on a contact, if present.
fn prop<'a>(c: &'a ical::parser::vcard::component::VcardContact, name: &str) -> Option<&'a str> {
    c.properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_deref())
}

fn contacts_to_json(contacts: &[ical::parser::vcard::component::VcardContact]) -> Option<String> {
    // Output key -> vCard property name.
    let fields = [("fn", "FN"), ("email", "EMAIL"), ("tel", "TEL"), ("org", "ORG")];
    let arr: Vec<serde_json::Value> = contacts
        .iter()
        .map(|c| {
            let mut obj = serde_json::Map::new();
            for (out_key, vcard_name) in fields {
                if let Some(v) = prop(c, vcard_name) {
                    obj.insert(out_key.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
}

fn names_to_json(contacts: &[ical::parser::vcard::component::VcardContact]) -> Option<String> {
    let arr: Vec<serde_json::Value> = contacts
        .iter()
        .filter_map(|c| prop(c, "FN").map(|s| serde_json::Value::String(s.to_string())))
        .collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
}

fn opt_text(o: Option<String>) -> NeutralValue {
    match o {
        Some(s) => NeutralValue::Text(s),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "vcard";
    version = env!("CARGO_PKG_VERSION");

    scalar vcard_count(text) -> int64 [propagate, deterministic] = |args| {
        Ok(match parse_contacts(&args.arg_text(0, "vcard_count")?) {
            Some(c) => NeutralValue::Int64(c.len() as i64),
            None => NeutralValue::Null,
        })
    };
    scalar vcard_to_json(text) -> text [propagate, deterministic] = |args| {
        Ok(match parse_contacts(&args.arg_text(0, "vcard_to_json")?) {
            Some(c) => opt_text(contacts_to_json(&c)),
            None => NeutralValue::Null,
        })
    };
    scalar vcard_names(text) -> text [propagate, deterministic] = |args| {
        Ok(match parse_contacts(&args.arg_text(0, "vcard_names")?) {
            Some(c) => opt_text(names_to_json(&c)),
            None => NeutralValue::Null,
        })
    };
}
