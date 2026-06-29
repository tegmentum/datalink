//! Neutral core for the `passphrase` extension — diceware-style
//! passphrase generation (via `chbs`, bundled wordlist) — written ONCE.
//! The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `passphrase() -> text` (e.g. "correct horse battery staple")
//!
//! NONDETERMINISTIC (wasi random).

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "passphrase";
    version = env!("CARGO_PKG_VERSION");

    scalar passphrase() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(chbs::passphrase()))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    #[test]
    fn nonempty_and_nondet() {
        let a = Core::dispatch(0, &[]).unwrap();
        match a {
            NeutralValue::Text(s) => assert!(!s.is_empty()),
            _ => panic!("expected text"),
        }
        assert!(!Core::DECLS[0].deterministic);
    }
}
