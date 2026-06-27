//! Neutral core for the `natsort` extension — natural-order string
//! comparison — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `natsort_compare(a, b) -> int64` — -1 / 0 / 1; orders "img2"
//!     before "img12".
//!
//! # Drift (documented; cross-DB reconciliation deferred)
//!
//! Both ports name this `natsort_compare`, but the SEMANTICS drifted:
//! ducklink uses `natord::compare` (case-sensitive); sqlink is
//! case-insensitive with leading-zero tie-breaks and also ships
//! `natsort_key` / `natsort_less`. This core adopts ducklink's behaviour
//! (the byte-parity gate is ducklink's committed smoke); the semantic
//! reconciliation + the two sqlink extras ride with the deferred sqlink
//! shim rather than silently changing ducklink's ordering.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "natsort";
    version = env!("CARGO_PKG_VERSION");

    scalar natsort_compare(text, text) -> int64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "natsort_compare")?;
        let b = args.arg_text(1, "natsort_compare")?;
        Ok(NeutralValue::Int64(match natord::compare(&a, &b) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }))
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
        assert_eq!(Core::dispatch(idx("natsort_compare"), &[t("img2"), t("img12")]).unwrap(), NeutralValue::Int64(-1));
        assert_eq!(Core::dispatch(idx("natsort_compare"), &[t("img12"), t("img2")]).unwrap(), NeutralValue::Int64(1));
        assert_eq!(Core::dispatch(idx("natsort_compare"), &[t("file10"), t("file10")]).unwrap(), NeutralValue::Int64(0));
        assert_eq!(Core::dispatch(idx("natsort_compare"), &[t("a100"), t("a99")]).unwrap(), NeutralValue::Int64(1));
    }
}
