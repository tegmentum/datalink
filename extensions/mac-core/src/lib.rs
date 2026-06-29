//! Neutral core for the `mac` extension — MAC address validation +
//! normalization via the `macaddr` crate — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `mac_valid(text) -> boolean`
//!   * `mac_normalize(text) -> text` — canonical colon-separated form;
//!     NULL if invalid.

extern crate alloc;

use alloc::string::ToString;
use core::str::FromStr;
use datalink_extcore::NeutralValue;
use macaddr::MacAddr;

datalink_extcore::declare! {
    core = Core;
    extension = "mac";
    version = env!("CARGO_PKG_VERSION");

    scalar mac_valid(text) -> boolean [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "mac_valid")?;
        Ok(NeutralValue::Boolean(MacAddr::from_str(&s).is_ok()))
    };

    scalar mac_normalize(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "mac_normalize")?;
        Ok(match MacAddr::from_str(&s) {
            Ok(m) => NeutralValue::Text(m.to_string()),
            Err(_) => NeutralValue::Null,
        })
    };
}
