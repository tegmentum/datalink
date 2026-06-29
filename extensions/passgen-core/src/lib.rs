//! Neutral core for the `passgen` extension — random password generation
//! (via `rand`, wasi entropy) — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `gen_password(length int64) -> text` (mixed alphanumeric + symbols)
//!   * `gen_password_alnum(length int64) -> text` (letters + digits only)
//!
//! NONDETERMINISTIC. `length` is clamped to 1..=256; NULL length -> 16.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use rand::Rng;

const FULL: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()-_=+[]{};:,.?";
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Resolve the length arg: Int64 -> clamp(1, 256); anything else (incl. NULL
/// or absent) -> the default 16. Matches the pre-pullup hand-written behaviour.
fn len_arg(args: &[NeutralValue]) -> usize {
    match args.first() {
        Some(NeutralValue::Int64(n)) => (*n).clamp(1, 256) as usize,
        _ => 16,
    }
}

fn generate(len: usize, set: &[u8]) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| set[rng.gen_range(0..set.len())] as char)
        .collect()
}

datalink_extcore::declare! {
    core = Core;
    extension = "passgen";
    version = env!("CARGO_PKG_VERSION");

    scalar gen_password(int64) -> text [called, nondeterministic] = |args| {
        Ok(NeutralValue::Text(generate(len_arg(args), FULL)))
    };

    scalar gen_password_alnum(int64) -> text [called, nondeterministic] = |args| {
        Ok(NeutralValue::Text(generate(len_arg(args), ALNUM)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    #[test]
    fn length_and_charset() {
        match Core::dispatch(0, &[NeutralValue::Int64(20)]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.chars().count(), 20),
            _ => panic!("expected text"),
        }
        match Core::dispatch(1, &[NeutralValue::Int64(12)]).unwrap() {
            NeutralValue::Text(s) => {
                assert_eq!(s.len(), 12);
                assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
            }
            _ => panic!("expected text"),
        }
        // NULL length -> default 16.
        match Core::dispatch(0, &[NeutralValue::Null]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.chars().count(), 16),
            _ => panic!("expected text"),
        }
    }
}
