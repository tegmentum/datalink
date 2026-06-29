//! Neutral core for the `humantime` extension — human-friendly durations (via
//! the `humantime` crate) — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `humantime_parse(text) -> int64`     seconds (NULL if unparseable)
//!   * `humantime_format(int64) -> text`    e.g. "1h 30m" (NULL if negative)
//!
//! NULL -> NULL.

extern crate alloc;

use core::time::Duration;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "humantime";
    version = env!("CARGO_PKG_VERSION");

    scalar humantime_parse(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "humantime_parse")?;
        Ok(match humantime::parse_duration(&s) {
            Ok(d) => NeutralValue::Int64(d.as_secs() as i64),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar humantime_format(int64) -> text [propagate, deterministic] = |args| {
        let secs = args.arg_int(0, "humantime_format")?;
        if secs < 0 { return Ok(NeutralValue::Null); }
        Ok(NeutralValue::Text(
            humantime::format_duration(Duration::from_secs(secs as u64)).to_string(),
        ))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("humantime_parse"), &[t("1h 30m")]).unwrap(), NeutralValue::Int64(5400));
        assert_eq!(Core::dispatch(idx("humantime_parse"), &[t("2 days")]).unwrap(), NeutralValue::Int64(172800));
        assert_eq!(Core::dispatch(idx("humantime_format"), &[NeutralValue::Int64(5400)]).unwrap(), t("1h 30m"));
        assert_eq!(Core::dispatch(idx("humantime_parse"), &[t("not a duration")]).unwrap(), NeutralValue::Null);
    }
}
