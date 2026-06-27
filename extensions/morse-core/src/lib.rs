//! Neutral core for the `morse` extension — International Morse code —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `morse_encode(text) -> text` — letters joined by spaces, words by " / "
//!   * `morse_decode(text) -> text`
//!
//! # Reconciled drift
//!
//! Both ports expose the same two functions. The behaviour differs only
//! on UNKNOWN characters: ducklink DROPS them, sqlink substitutes `'?'`.
//! This core adopts ducklink's drop-unknown behaviour (the byte-parity
//! gate is ducklink's committed smoke); the cross-DB behavioural
//! reconciliation rides with the deferred sqlink shim.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Morse logic (DB-agnostic), byte-for-byte the ducklink algorithm.
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub const TABLE: &[(char, &str)] = &[
        ('A', ".-"), ('B', "-..."), ('C', "-.-."), ('D', "-.."), ('E', "."), ('F', "..-."),
        ('G', "--."), ('H', "...."), ('I', ".."), ('J', ".---"), ('K', "-.-"), ('L', ".-.."),
        ('M', "--"), ('N', "-."), ('O', "---"), ('P', ".--."), ('Q', "--.-"), ('R', ".-."),
        ('S', "..."), ('T', "-"), ('U', "..-"), ('V', "...-"), ('W', ".--"), ('X', "-..-"),
        ('Y', "-.--"), ('Z', "--.."),
        ('0', "-----"), ('1', ".----"), ('2', "..---"), ('3', "...--"), ('4', "....-"),
        ('5', "....."), ('6', "-...."), ('7', "--..."), ('8', "---.."), ('9', "----."),
        ('.', ".-.-.-"), (',', "--..--"), ('?', "..--.."), ('\'', ".----."), ('!', "-.-.--"),
        ('/', "-..-."), ('(', "-.--."), (')', "-.--.-"), ('&', ".-..."), (':', "---..."),
        (';', "-.-.-."), ('=', "-...-"), ('+', ".-.-."), ('-', "-....-"), ('_', "..--.-"),
        ('"', ".-..-."), ('$', "...-..-"), ('@', ".--.-."),
    ];

    pub fn encode(text: &str) -> String {
        text.split_whitespace()
            .map(|word| {
                word.chars()
                    .filter_map(|c| {
                        let u = c.to_ascii_uppercase();
                        TABLE.iter().find(|(k, _)| *k == u).map(|(_, v)| *v)
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    pub fn decode(code: &str) -> String {
        code.split(" / ")
            .map(|word| {
                word.split_whitespace()
                    .filter_map(|sym| TABLE.iter().find(|(_, v)| *v == sym).map(|(k, _)| *k))
                    .collect::<String>()
            })
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "morse";
    version = env!("CARGO_PKG_VERSION");

    scalar morse_encode(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::encode(&args.arg_text(0, "morse_encode")?)))
    };
    scalar morse_decode(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::decode(&args.arg_text(0, "morse_decode")?)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("morse_encode"), &[t("SOS")]).unwrap(), t("... --- ..."));
        assert_eq!(Core::dispatch(idx("morse_encode"), &[t("Hello World")]).unwrap(),
            t(".... . .-.. .-.. --- / .-- --- .-. .-.. -.."));
        assert_eq!(Core::dispatch(idx("morse_decode"), &[t("... --- ...")]).unwrap(), t("SOS"));
        assert_eq!(Core::dispatch(idx("morse_decode"), &[t(".... .. / - .... . .-. .")]).unwrap(), t("HI THERE"));
    }
}
