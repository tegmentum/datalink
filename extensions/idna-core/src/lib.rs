//! Neutral core for the `idna` extension — IDNA / punycode domain conversion
//! (via the `idna` crate) — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `idna_to_ascii(text) -> text`    (punycode; NULL on error)
//!   * `idna_to_unicode(text) -> text`  (best-effort decode)
//!
//! NULL -> NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "idna";
    version = env!("CARGO_PKG_VERSION");

    scalar idna_to_ascii(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "idna_to_ascii")?;
        Ok(match idna::domain_to_ascii(&s) {
            Ok(a) => NeutralValue::Text(a),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar idna_to_unicode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "idna_to_unicode")?;
        Ok(NeutralValue::Text(idna::domain_to_unicode(&s).0))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("idna_to_ascii"), &[t("münchen.de")]).unwrap(), t("xn--mnchen-3ya.de"));
        assert_eq!(Core::dispatch(idx("idna_to_unicode"), &[t("xn--mnchen-3ya.de")]).unwrap(), t("münchen.de"));
        assert_eq!(Core::dispatch(idx("idna_to_ascii"), &[t("пример.рф")]).unwrap(), t("xn--e1afmkfd.xn--p1ai"));
    }
}
