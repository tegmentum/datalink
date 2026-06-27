//! Neutral core for the `humansize` extension — human-readable byte
//! sizes — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `humansize(int64) -> text` — decimal, e.g. "1.50 MB"
//!   * `humansize_binary(int64) -> text` — binary, e.g. "1.00 MiB"
//!
//! NULL / negative input → NULL.
//!
//! # Drift (documented; union deferred)
//!
//! sqlink ships an entirely different name family (`humansize_bytes` /
//! `humansize_ibytes` / `humansize_duration` / `humansize_parse_*`) with
//! NO name overlap. This core pulls up ducklink's surface; the sqlink
//! family rides with the deferred sqlink shim.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;
use humansize::{format_size, BINARY, DECIMAL};

datalink_extcore::declare! {
    core = Core;
    extension = "humansize";
    version = env!("CARGO_PKG_VERSION");

    scalar humansize(int64) -> text [propagate, deterministic] = |args| {
        Ok(match u64::try_from(args.arg_int(0, "humansize")?) {
            Ok(n) => NeutralValue::Text(format_size(n, DECIMAL)),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar humansize_binary(int64) -> text [propagate, deterministic] = |args| {
        Ok(match u64::try_from(args.arg_int(0, "humansize_binary")?) {
            Ok(n) => NeutralValue::Text(format_size(n, BINARY)),
            Err(_) => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn i(n: i64) -> NeutralValue { NeutralValue::Int64(n) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("humansize"), &[i(1500000)]).unwrap(), NeutralValue::Text(String::from("1.50 MB")));
        assert_eq!(Core::dispatch(idx("humansize_binary"), &[i(1048576)]).unwrap(), NeutralValue::Text(String::from("1 MiB")));
        assert_eq!(Core::dispatch(idx("humansize"), &[i(0)]).unwrap(), NeutralValue::Text(String::from("0 B")));
        assert_eq!(Core::dispatch(idx("humansize"), &[i(999)]).unwrap(), NeutralValue::Text(String::from("999 B")));
        assert_eq!(Core::dispatch(idx("humansize"), &[i(-5)]).unwrap(), NeutralValue::Null);
    }
}
