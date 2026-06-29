//! Neutral core for the `nato` extension — NATO phonetic alphabet
//! spelling — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `nato(text) -> text` — letters map to Alfa/Bravo/..., digits to
//!     One/Two/..., joined by spaces; other characters are dropped.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const ALPHA: [&str; 26] = [
        "Alfa", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India",
        "Juliett", "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo",
        "Sierra", "Tango", "Uniform", "Victor", "Whiskey", "Xray", "Yankee", "Zulu",
    ];
    pub const DIGIT: [&str; 10] = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Niner",
    ];

    pub fn nato(text: &str) -> String {
        text.chars()
            .filter_map(|c| {
                if c.is_ascii_alphabetic() {
                    Some(ALPHA[(c.to_ascii_uppercase() as u8 - b'A') as usize])
                } else if c.is_ascii_digit() {
                    Some(DIGIT[(c as u8 - b'0') as usize])
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "nato";
    version = env!("CARGO_PKG_VERSION");

    scalar nato(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nato")?;
        Ok(NeutralValue::Text(logic::nato(&s)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    #[test]
    fn spells_letters_and_digits() {
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Text(String::from("A1!"))]).unwrap(),
            NeutralValue::Text(String::from("Alfa One"))
        );
    }
}
