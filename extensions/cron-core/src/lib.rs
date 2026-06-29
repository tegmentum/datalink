//! Neutral core for the `cron` extension — deterministic UTC cron-expression
//! evaluation (via `croner` + `chrono`; the reference time is always an
//! argument, never a clock read) — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `cron_is_valid(expr)            -> boolean`
//!   * `cron_next(expr, after_unix_ms) -> int64`  next fire strictly after (UTC ms)
//!   * `cron_prev(expr, before_unix_ms)-> int64`  previous fire strictly before
//!
//! NULL / invalid expr -> NULL, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): `croner` is a std crate; `extern crate alloc` keeps
//! the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use chrono::{DateTime, TimeZone, Utc};
use croner::Cron;
use datalink_extcore::NeutralValue;
use core::str::FromStr;

/// Parse a cron expression (DB-agnostic).
pub fn parse_cron(expr: &str) -> Option<Cron> {
    Cron::from_str(expr).ok()
}

/// UTC ms -> DateTime (DB-agnostic).
pub fn ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    match Utc.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => Some(dt),
        _ => None,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "cron";
    version = env!("CARGO_PKG_VERSION");

    scalar cron_is_valid(text) -> boolean [propagate, deterministic] = |args| {
        let expr = args.arg_text(0, "cron_is_valid")?;
        Ok(NeutralValue::Boolean(parse_cron(&expr).is_some()))
    };

    scalar cron_next(text, int64) -> int64 [propagate, deterministic] = |args| {
        let expr = args.arg_text(0, "cron_next")?;
        let ms = args.arg_int(1, "cron_next")?;
        Ok(match (parse_cron(&expr), ms_to_dt(ms)) {
            (Some(cron), Some(dt)) => match cron.find_next_occurrence(&dt, false) {
                Ok(fire) => NeutralValue::Int64(fire.timestamp_millis()),
                Err(_) => NeutralValue::Null,
            },
            _ => NeutralValue::Null,
        })
    };

    scalar cron_prev(text, int64) -> int64 [propagate, deterministic] = |args| {
        let expr = args.arg_text(0, "cron_prev")?;
        let ms = args.arg_int(1, "cron_prev")?;
        Ok(match (parse_cron(&expr), ms_to_dt(ms)) {
            (Some(cron), Some(dt)) => match cron.find_previous_occurrence(&dt, false) {
                Ok(fire) => NeutralValue::Int64(fire.timestamp_millis()),
                Err(_) => NeutralValue::Null,
            },
            _ => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        // Reference ms = 1700000000000 (2023-11-14T22:13:20Z); daily midnight.
        assert_eq!(
            Core::dispatch(idx("cron_next"), &[t("0 0 * * *"), NeutralValue::Int64(1700000000000)]).unwrap(),
            NeutralValue::Int64(1700006400000)
        );
        assert_eq!(
            Core::dispatch(idx("cron_prev"), &[t("0 0 * * *"), NeutralValue::Int64(1700000000000)]).unwrap(),
            NeutralValue::Int64(1699920000000)
        );
        assert_eq!(
            Core::dispatch(idx("cron_is_valid"), &[t("* * * * *")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("cron_is_valid"), &[t("bad")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }
}
