//! Neutral core for the `semver` extension — semantic-version
//! parsing/comparison — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `semver_valid(text) -> boolean`
//!   * `semver_major / semver_minor / semver_patch(text) -> int64`
//!   * `semver_compare(a, b) -> int64`  (-1 / 0 / 1)
//!
//! Invalid input → NULL (or `false` for `semver_valid`).
//!
//! # Drift (documented; union deferred)
//!
//! sqlink renamed the validator (`semver_validate`) and added
//! `semver_pre` / `semver_build` / `semver_max` / `semver_satisfies` /
//! `semver_increment`. That union (and the validate-name reconciliation)
//! rides with the deferred sqlink shim; this core pulls up ducklink's
//! surface verbatim.

#![no_std]

extern crate alloc;

use core::cmp::Ordering;
use datalink_extcore::NeutralValue;
use semver::Version;

datalink_extcore::declare! {
    core = Core;
    extension = "semver";
    version = env!("CARGO_PKG_VERSION");

    scalar semver_valid(text) -> boolean [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "semver_valid")?;
        Ok(NeutralValue::Boolean(Version::parse(&s).is_ok()))
    };
    scalar semver_major(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "semver_major")?;
        Ok(match Version::parse(&s) {
            Ok(v) => NeutralValue::Int64(v.major as i64),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar semver_minor(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "semver_minor")?;
        Ok(match Version::parse(&s) {
            Ok(v) => NeutralValue::Int64(v.minor as i64),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar semver_patch(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "semver_patch")?;
        Ok(match Version::parse(&s) {
            Ok(v) => NeutralValue::Int64(v.patch as i64),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar semver_compare(text, text) -> int64 [propagate, deterministic] = |args| {
        let a = Version::parse(&args.arg_text(0, "semver_compare")?).ok();
        let b = Version::parse(&args.arg_text(1, "semver_compare")?).ok();
        Ok(match (a, b) {
            (Some(a), Some(b)) => NeutralValue::Int64(match a.cmp(&b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }),
            _ => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("semver_valid"), &[t("1.2.3")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("semver_valid"), &[t("not.a.version")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("semver_major"), &[t("2.5.9")]).unwrap(), NeutralValue::Int64(2));
        assert_eq!(Core::dispatch(idx("semver_minor"), &[t("2.5.9")]).unwrap(), NeutralValue::Int64(5));
        assert_eq!(Core::dispatch(idx("semver_patch"), &[t("2.5.9")]).unwrap(), NeutralValue::Int64(9));
        assert_eq!(Core::dispatch(idx("semver_compare"), &[t("1.0.0"), t("1.0.1")]).unwrap(), NeutralValue::Int64(-1));
        assert_eq!(Core::dispatch(idx("semver_compare"), &[t("2.0.0"), t("2.0.0")]).unwrap(), NeutralValue::Int64(0));
        assert_eq!(Core::dispatch(idx("semver_compare"), &[t("3.1.0"), t("3.0.9")]).unwrap(), NeutralValue::Int64(1));
    }
}
