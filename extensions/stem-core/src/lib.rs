//! Neutral core for the `stem` extension — Snowball/Porter word stemming (via
//! `rust-stemmers`) — written ONCE.
//!
//!   * `stem(word, language) -> text`. language is a name or ISO 639-1 code
//!     (english/fr/de/es/it/pt/ru/nl/sv/no/da/fi; default english).

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;
    use rust_stemmers::{Algorithm, Stemmer};

    /// Map a language name / ISO 639-1 code to a Snowball algorithm
    /// (unknown -> English), byte-for-byte the pre-pullup mapping.
    pub fn algorithm(lang: &str) -> Algorithm {
        match lang.trim().to_ascii_lowercase().as_str() {
            "french" | "fr" => Algorithm::French,
            "german" | "de" => Algorithm::German,
            "spanish" | "es" => Algorithm::Spanish,
            "italian" | "it" => Algorithm::Italian,
            "portuguese" | "pt" => Algorithm::Portuguese,
            "russian" | "ru" => Algorithm::Russian,
            "dutch" | "nl" => Algorithm::Dutch,
            "swedish" | "sv" => Algorithm::Swedish,
            "norwegian" | "no" => Algorithm::Norwegian,
            "danish" | "da" => Algorithm::Danish,
            "finnish" | "fi" => Algorithm::Finnish,
            _ => Algorithm::English,
        }
    }

    /// Stem `word` (lowercased first) under `lang`.
    pub fn stem(word: &str, lang: &str) -> String {
        let stemmer = Stemmer::create(algorithm(lang));
        stemmer.stem(&word.to_lowercase()).into_owned()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "stem";
    version = env!("CARGO_PKG_VERSION");

    scalar stem(text, text) -> text [propagate, deterministic] = |args| {
        let word = args.arg_text(0, "stem")?;
        let lang = args.arg_text(1, "stem")?;
        Ok(NeutralValue::Text(logic::stem(&word, &lang)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    #[test]
    fn stems_english() {
        let i = Core::DECLS.iter().position(|d| d.name == "stem").unwrap();
        assert_eq!(
            Core::dispatch(
                i,
                &[
                    NeutralValue::Text("running".to_string()),
                    NeutralValue::Text("english".to_string())
                ]
            )
            .unwrap(),
            NeutralValue::Text("run".to_string())
        );
    }
}
