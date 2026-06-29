//! Neutral core for the `phonetic2` extension — additional phonetic
//! algorithms (nysiis, refined_soundex, double_metaphone primary code) via
//! `rphonetic` — written ONCE. Complements `phonetic` (soundex/metaphone).
//! The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.

extern crate alloc;

use datalink_extcore::NeutralValue;
use rphonetic::{DoubleMetaphone, Encoder, Nysiis, RefinedSoundex};

datalink_extcore::declare! {
    core = Core;
    extension = "phonetic2";
    version = env!("CARGO_PKG_VERSION");

    scalar nysiis(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nysiis")?;
        Ok(NeutralValue::Text(Nysiis::default().encode(&s)))
    };

    scalar refined_soundex(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "refined_soundex")?;
        Ok(NeutralValue::Text(RefinedSoundex::default().encode(&s)))
    };

    scalar double_metaphone(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "double_metaphone")?;
        Ok(NeutralValue::Text(DoubleMetaphone::default().encode(&s)))
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
            NeutralValue::Text(String::from("RABAD"))
        );
        assert_eq!(
            Core::dispatch(2, &[NeutralValue::Text(String::from("Thompson"))]).unwrap(),
            NeutralValue::Text(String::from("TMPS"))
        );
    }
}
