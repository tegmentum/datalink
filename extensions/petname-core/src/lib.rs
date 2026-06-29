//! Neutral core for the `petname` extension — memorable random names
//! (via the `petname` crate) — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `petname(words int64, separator text) -> text` (e.g. "wise-firm-cat")
//!
//! NONDETERMINISTIC (wasi random). `words` is clamped to 1..=10 (default 2);
//! the separator defaults to "-". NULL on generation failure.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "petname";
    version = env!("CARGO_PKG_VERSION");

    scalar petname(int64, text) -> text [called, nondeterministic] = |args| {
        let words = match args.first() {
            Some(NeutralValue::Int64(n)) if *n >= 1 && *n <= 10 => *n as u8,
            _ => 2,
        };
        let sep = match args.get(1) {
            Some(NeutralValue::Text(s)) => s.clone(),
            _ => String::from("-"),
        };
        Ok(match petname::petname(words, &sep) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    #[test]
    fn generates_with_sep() {
        match Core::dispatch(0, &[NeutralValue::Int64(2), NeutralValue::Text(String::from("."))]).unwrap() {
            NeutralValue::Text(s) => assert!(s.contains('.')),
            _ => panic!("expected text"),
        }
    }
}
