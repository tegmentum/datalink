//! Neutral core for the `rot47` extension — ROT47 cipher (the 94 printable
//! ASCII chars 0x21..=0x7E rotated by 47; self-inverse) — written ONCE.
//!
//!   * `rot47(text) -> text`.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;

    /// Rotate every printable-ASCII char (0x21..=0x7E) by 47, leaving all
    /// others untouched. Self-inverse.
    pub fn rot47(s: &str) -> String {
        s.chars()
            .map(|c| {
                let b = c as u32;
                if (0x21..=0x7E).contains(&b) {
                    char::from_u32(0x21 + (b - 0x21 + 47) % 94).unwrap_or(c)
                } else {
                    c
                }
            })
            .collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "rot47";
    version = env!("CARGO_PKG_VERSION");

    scalar rot47(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "rot47")?;
        Ok(NeutralValue::Text(logic::rot47(&s)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::string::String;

    #[test]
    fn self_inverse() {
        let i = Core::DECLS.iter().position(|d| d.name == "rot47").unwrap();
        let once = Core::dispatch(i, &[NeutralValue::Text(String::from("Hello World!"))]).unwrap();
        let twice = Core::dispatch(i, &[once.clone()]).unwrap();
        assert_eq!(twice, NeutralValue::Text(String::from("Hello World!")));
    }
}
