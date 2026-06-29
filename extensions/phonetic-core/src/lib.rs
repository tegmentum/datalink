//! Neutral core for the `phonetic` extension — soundex + metaphone
//! phonetic codes (via `rphonetic`) — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `soundex(text) -> text`
//!   * `metaphone(text) -> text`

extern crate alloc;

use datalink_extcore::NeutralValue;
use rphonetic::{Encoder, Metaphone, Soundex};

datalink_extcore::declare! {
    core = Core;
    extension = "phonetic";
    version = env!("CARGO_PKG_VERSION");

    scalar soundex(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "soundex")?;
        Ok(NeutralValue::Text(Soundex::default().encode(&s)))
    };

    scalar metaphone(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "metaphone")?;
        Ok(NeutralValue::Text(Metaphone::default().encode(&s)))
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
    fn codes() {
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Text(String::from("Robert"))]).unwrap(),
            NeutralValue::Text(String::from("R163"))
        );
        assert_eq!(
            Core::dispatch(1, &[NeutralValue::Text(String::from("Thompson"))]).unwrap(),
            NeutralValue::Text(String::from("0MPS"))
        );
    }
}
