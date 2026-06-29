//! Neutral core for the `ordinal` extension — ordinal-number suffixing
//! ("1st", "2nd", "3rd", "11th", "21st", "-4th") — written ONCE. The
//! per-DB shim is generated from the [`declare!`](datalink_extcore::declare)
//! table below.
//!
//!   * `ordinal(n int64) -> text`

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::format;
    use alloc::string::String;

    pub fn ordinal(n: i64) -> String {
        let last2 = (n.unsigned_abs() % 100) as u64;
        let suffix = if (11..=13).contains(&last2) {
            "th"
        } else {
            match last2 % 10 {
                1 => "st",
                2 => "nd",
                3 => "rd",
                _ => "th",
            }
        };
        format!("{}{}", n, suffix)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ordinal";
    version = env!("CARGO_PKG_VERSION");

    scalar ordinal(int64) -> text [propagate, deterministic] = |args| {
        let n = args.arg_int(0, "ordinal")?;
        Ok(NeutralValue::Text(logic::ordinal(n)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;
    use std::vec;

    #[test]
    fn suffixes() {
        for (n, want) in [(1, "1st"), (2, "2nd"), (3, "3rd"), (11, "11th"), (21, "21st"), (113, "113th")] {
            assert_eq!(
                Core::dispatch(0, &[NeutralValue::Int64(n)]).unwrap(),
                NeutralValue::Text(String::from(want))
            );
        }
    }
}
