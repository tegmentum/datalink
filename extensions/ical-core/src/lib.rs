//! Neutral core for the `ical` extension — iCalendar (.ics) parsing via
//! the `ical` crate — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ical_event_count(ics) -> int64`  (VEVENTs across all VCALENDARs)
//!   * `ical_to_json(ics) -> text`       ([{summary,dtstart,dtend,uid}, ...])
//!   * `ical_summaries(ics) -> text`     (["summary", ...])
//!
//! Parse error / non-text input -> NULL (never panics). The surface is
//! identical in both ports (zero drift).

extern crate alloc;

use datalink_extcore::NeutralValue;
use ical::IcalParser;

/// Parse every VCALENDAR in `ics`, returning all VEVENTs flattened.
/// Any parse error in any calendar block -> None (whole call yields NULL).
fn parse_events(ics: &str) -> Option<Vec<ical::parser::ical::component::IcalEvent>> {
    let mut events = Vec::new();
    for cal in IcalParser::new(ics.as_bytes()) {
        let cal = cal.ok()?;
        events.extend(cal.events);
    }
    Some(events)
}

/// First value of a named property on an event, if present.
fn prop<'a>(ev: &'a ical::parser::ical::component::IcalEvent, name: &str) -> Option<&'a str> {
    ev.properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_deref())
}

fn events_to_json(events: &[ical::parser::ical::component::IcalEvent]) -> Option<String> {
    let arr: Vec<serde_json::Value> = events
        .iter()
        .map(|ev| {
            let mut obj = serde_json::Map::new();
            for key in ["summary", "dtstart", "dtend", "uid"] {
                if let Some(v) = prop(ev, key) {
                    obj.insert(key.to_string(), serde_json::Value::String(v.to_string()));
                }
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::to_string(&serde_json::Value::Array(arr)).ok()
}

fn summaries_to_json(events: &[ical::parser::ical::component::IcalEvent]) -> Option<String> {
    let arr: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|ev| prop(ev, "summary").map(|s| serde_json::Value::String(s.to_string())))
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
    extension = "ical";
    version = env!("CARGO_PKG_VERSION");

    scalar ical_event_count(text) -> int64 [propagate, deterministic] = |args| {
        Ok(match parse_events(&args.arg_text(0, "ical_event_count")?) {
            Some(e) => NeutralValue::Int64(e.len() as i64),
            None => NeutralValue::Null,
        })
    };
    scalar ical_to_json(text) -> text [propagate, deterministic] = |args| {
        Ok(match parse_events(&args.arg_text(0, "ical_to_json")?) {
            Some(e) => opt_text(events_to_json(&e)),
            None => NeutralValue::Null,
        })
    };
    scalar ical_summaries(text) -> text [propagate, deterministic] = |args| {
        Ok(match parse_events(&args.arg_text(0, "ical_summaries")?) {
            Some(e) => opt_text(summaries_to_json(&e)),
            None => NeutralValue::Null,
        })
    };
}
