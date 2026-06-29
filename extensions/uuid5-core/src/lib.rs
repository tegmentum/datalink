//! Neutral core for the `uuid5` extension — namespace (name-based) UUIDs —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `uuid_v5(namespace, name) -> text`
//!   * `uuid_v3(namespace, name) -> text`
//!
//! `namespace` is a UUID string or a well-known alias (dns / url / oid /
//! x500). Deterministic. Bad namespace / `NULL -> NULL`.

extern crate alloc;

use datalink_extcore::NeutralValue;
use uuid::Uuid;

/// Resolve a namespace alias or UUID string to a `Uuid`. None if neither.
pub fn namespace(s: &str) -> Option<Uuid> {
    match s.trim().to_ascii_lowercase().as_str() {
        "dns" => Some(Uuid::NAMESPACE_DNS),
        "url" => Some(Uuid::NAMESPACE_URL),
        "oid" => Some(Uuid::NAMESPACE_OID),
        "x500" => Some(Uuid::NAMESPACE_X500),
        _ => Uuid::parse_str(s.trim()).ok(),
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "uuid5";
    version = env!("CARGO_PKG_VERSION");

    scalar uuid_v5(text, text) -> text [propagate, deterministic] = |args| {
        let ns = args.arg_text(0, "uuid_v5")?;
        let name = args.arg_text(1, "uuid_v5")?;
        Ok(match namespace(&ns) {
            Some(n) => NeutralValue::Text(Uuid::new_v5(&n, name.as_bytes()).to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar uuid_v3(text, text) -> text [propagate, deterministic] = |args| {
        let ns = args.arg_text(0, "uuid_v3")?;
        let name = args.arg_text(1, "uuid_v3")?;
        Ok(match namespace(&ns) {
            Some(n) => NeutralValue::Text(Uuid::new_v3(&n, name.as_bytes()).to_string()),
            None => NeutralValue::Null,
        })
    };
}
