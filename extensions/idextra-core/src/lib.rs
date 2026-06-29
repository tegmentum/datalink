//! Neutral core for the `idextra` extension — modern identifier generators —
//! written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ksuid() -> text`  (NONDETERMINISTIC; svix-ksuid, wasi random + clock)
//!   * `cuid2() -> text`  (NONDETERMINISTIC; cuid2, wasi random)
//!
//! Both are declared `nondeterministic`, so the generated shim omits
//! `Funcflags::DETERMINISTIC` and the DuckDB optimizer never folds them.
//! These complement `ids` (ulid/nanoid) and the uuid scalars.

extern crate alloc;

use alloc::string::ToString;
use datalink_extcore::NeutralValue;
use svix_ksuid::{Ksuid, KsuidLike};

datalink_extcore::declare! {
    core = Core;
    extension = "idextra";
    version = env!("CARGO_PKG_VERSION");

    scalar ksuid() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(Ksuid::now(None).to_string()))
    };
    scalar cuid2() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(cuid2::create_id()))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn shape() {
        match Core::dispatch(idx("ksuid"), &[]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.len(), 27),
            o => panic!("{o:?}"),
        }
        match Core::dispatch(idx("cuid2"), &[]).unwrap() {
            NeutralValue::Text(s) => assert!(!s.is_empty()),
            o => panic!("{o:?}"),
        }
    }
}
