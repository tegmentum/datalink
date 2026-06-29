//! Neutral core for the `initials` extension — initials / acronyms — written
//! ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `initials(text) -> text`         first letter of each word, uppercased.
//!   * `initials_dotted(text) -> text`  the same, each followed by a dot.
//!
//! NULL -> NULL (propagate).

#![no_std]

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::vec::Vec;

    /// First alphanumeric char of each whitespace-separated word, uppercased.
    pub fn first_letters(s: &str) -> Vec<char> {
        s.split_whitespace()
            .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_ascii_uppercase()))
            .collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "initials";
    version = env!("CARGO_PKG_VERSION");

    scalar initials(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "initials")?;
        Ok(NeutralValue::Text(logic::first_letters(&s).into_iter().collect()))
    };
    scalar initials_dotted(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "initials_dotted")?;
        let mut o = String::new();
        for c in logic::first_letters(&s) { o.push(c); o.push('.'); }
        Ok(NeutralValue::Text(o))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("initials"), &[t("Portable Document Format")]).unwrap(), t("PDF"));
        assert_eq!(Core::dispatch(idx("initials_dotted"), &[t("Thomas Stearns Eliot")]).unwrap(), t("T.S.E."));
        assert_eq!(Core::dispatch(idx("initials"), &[t("   ")]).unwrap(), t(""));
    }
}
