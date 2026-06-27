//! Neutral core for the `faker` extension — fake data generation via the
//! `fake` crate (bundled data lists) — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `fake_name() -> text`
//!   * `fake_email() -> text`
//!   * `fake_username() -> text`
//!   * `fake_city() -> text`
//!   * `fake_company() -> text`
//!
//! All NONDETERMINISTIC (wasi random); useful for seeding test datasets.
//! Declared `nondeterministic`, so the generated shim omits
//! `Funcflags::DETERMINISTIC` and the optimizer treats them as volatile.
//! The surface is identical in both ports (zero drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use fake::faker::address::en::CityName;
use fake::faker::company::en::CompanyName;
use fake::faker::internet::en::{SafeEmail, Username};
use fake::faker::name::en::Name;
use fake::Fake;

datalink_extcore::declare! {
    core = Core;
    extension = "faker";
    version = env!("CARGO_PKG_VERSION");

    scalar fake_name() -> text [propagate, nondeterministic] = |_args| {
        let s: String = Name().fake();
        Ok(NeutralValue::Text(s))
    };
    scalar fake_email() -> text [propagate, nondeterministic] = |_args| {
        let s: String = SafeEmail().fake();
        Ok(NeutralValue::Text(s))
    };
    scalar fake_username() -> text [propagate, nondeterministic] = |_args| {
        let s: String = Username().fake();
        Ok(NeutralValue::Text(s))
    };
    scalar fake_city() -> text [propagate, nondeterministic] = |_args| {
        let s: String = CityName().fake();
        Ok(NeutralValue::Text(s))
    };
    scalar fake_company() -> text [propagate, nondeterministic] = |_args| {
        let s: String = CompanyName().fake();
        Ok(NeutralValue::Text(s))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn shape_and_determinism() {
        for n in ["fake_name", "fake_email", "fake_username", "fake_city", "fake_company"] {
            match Core::dispatch(idx(n), &[]).unwrap() {
                NeutralValue::Text(s) => assert!(!s.is_empty()),
                o => panic!("{n}: {o:?}"),
            }
            assert!(!Core::DECLS[idx(n)].deterministic, "{n} must be nondeterministic");
        }
    }
}
