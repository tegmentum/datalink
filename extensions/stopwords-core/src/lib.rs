//! Neutral core for the `stopwords` extension — stop-word detection + removal
//! (via the `stop-words` crate) — written ONCE.
//!
//!   * `is_stopword(word, language) -> boolean` (NULL on unknown language).
//!   * `remove_stopwords(text, language) -> text` (NULL on unknown language).

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Allowlist of ISO 639-1 codes `stop_words::get` accepts (it PANICS
    /// otherwise). `None` -> unknown language. Byte-for-byte the pre-pullup map.
    pub fn lang_code(s: &str) -> Option<&'static str> {
        match s.trim().to_ascii_lowercase().as_str() {
            "english" | "en" | "" => Some("en"),
            "french" | "fr" => Some("fr"),
            "german" | "de" => Some("de"),
            "spanish" | "es" => Some("es"),
            "italian" | "it" => Some("it"),
            "portuguese" | "pt" => Some("pt"),
            "dutch" | "nl" => Some("nl"),
            "russian" | "ru" => Some("ru"),
            "finnish" | "fi" => Some("fi"),
            "danish" | "da" => Some("da"),
            "swedish" | "sv" => Some("sv"),
            _ => None,
        }
    }

    /// `Some(true/false)`; `None` for an unknown language.
    pub fn is_stopword(word: &str, lang: &str) -> Option<bool> {
        let code = lang_code(lang)?;
        let list = stop_words::get(code);
        let w = word.to_ascii_lowercase();
        Some(list.iter().any(|s| *s == w))
    }

    /// Whitespace-tokenize, drop stop words (case-insensitive), re-join with a
    /// single space. `None` for an unknown language.
    pub fn remove_stopwords(text: &str, lang: &str) -> Option<String> {
        let code = lang_code(lang)?;
        let list = stop_words::get(code);
        let kept: Vec<&str> = text
            .split_whitespace()
            .filter(|w| {
                let lw = w.to_ascii_lowercase();
                !list.iter().any(|s| *s == lw)
            })
            .collect();
        Some(kept.join(" "))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "stopwords";
    version = env!("CARGO_PKG_VERSION");

    scalar is_stopword(text, text) -> boolean [propagate, deterministic] = |args| {
        let word = args.arg_text(0, "is_stopword")?;
        let lang = args.arg_text(1, "is_stopword")?;
        Ok(match logic::is_stopword(&word, &lang) {
            Some(b) => NeutralValue::Boolean(b),
            None => NeutralValue::Null,
        })
    };

    scalar remove_stopwords(text, text) -> text [propagate, deterministic] = |args| {
        let text = args.arg_text(0, "remove_stopwords")?;
        let lang = args.arg_text(1, "remove_stopwords")?;
        Ok(match logic::remove_stopwords(&text, &lang) {
            Some(t) => NeutralValue::Text(t),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(s.to_string())
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn detects_and_removes() {
        assert_eq!(
            Core::dispatch(idx("is_stopword"), &[t("the"), t("english")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("is_stopword"), &[t("x"), t("klingon")]).unwrap(),
            NeutralValue::Null
        );
        assert_eq!(
            Core::dispatch(
                idx("remove_stopwords"),
                &[t("the quick brown fox is on the run"), t("en")]
            )
            .unwrap(),
            t("quick brown fox")
        );
    }
}
