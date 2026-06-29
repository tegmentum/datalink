//! Neutral core for the `timezone` extension — IANA timezone lookups via
//! `chrono-tz` — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `tz_valid(name) -> boolean`                       is `name` a valid zone?
//!   * `tz_offset_seconds(name, unix_time) -> int64`     DST-aware UTC offset
//!   * `tz_abbreviation(name, unix_time) -> text`        e.g. EST/EDT
//!
//! Unknown zone / `NULL` -> `NULL` (tz_valid -> false). `tz_valid` is CALLED
//! (a NULL name yields false, not NULL), matching the pre-pullup behavior.

extern crate alloc;

use alloc::format;
use chrono::{Offset, TimeZone, Utc};
use chrono_tz::Tz;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "timezone";
    version = env!("CARGO_PKG_VERSION");

    scalar tz_valid(text) -> boolean [called, deterministic] = |args| {
        let name = args.arg_text(0, "tz_valid")?;
        let tz: Option<Tz> = name.trim().parse().ok();
        Ok(NeutralValue::Boolean(tz.is_some()))
    };

    scalar tz_offset_seconds(text, int64) -> int64 [propagate, deterministic] = |args| {
        let name = args.arg_text(0, "tz_offset_seconds")?;
        let tz: Tz = match name.trim().parse().ok() {
            Some(t) => t,
            None => return Ok(NeutralValue::Null),
        };
        let t = args.arg_int(1, "tz_offset_seconds")?;
        let ts = match Utc.timestamp_opt(t, 0).single() {
            Some(d) => d,
            None => return Ok(NeutralValue::Null),
        };
        let local = ts.with_timezone(&tz);
        Ok(NeutralValue::Int64(local.offset().fix().local_minus_utc() as i64))
    };

    scalar tz_abbreviation(text, int64) -> text [propagate, deterministic] = |args| {
        let name = args.arg_text(0, "tz_abbreviation")?;
        let tz: Tz = match name.trim().parse().ok() {
            Some(t) => t,
            None => return Ok(NeutralValue::Null),
        };
        let t = args.arg_int(1, "tz_abbreviation")?;
        let ts = match Utc.timestamp_opt(t, 0).single() {
            Some(d) => d,
            None => return Ok(NeutralValue::Null),
        };
        let local = ts.with_timezone(&tz);
        Ok(NeutralValue::Text(format!("{}", local.offset())))
    };
}
