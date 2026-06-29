//! Neutral core for the `elements` extension — periodic-table lookups (via
//! `mendeleev`), keyed by symbol — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `element_name(symbol)   -> text`
//!   * `element_number(symbol) -> int64`
//!   * `element_weight(symbol) -> float64`
//!
//! Unknown symbol -> `NULL`, byte-for-byte the pre-pullup behaviour.
//!
//! `std` (not `no_std`): `mendeleev` is a std crate; `extern crate alloc`
//! keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;
use mendeleev::Element;

/// Find an element by symbol, case-insensitively (DB-agnostic).
pub fn lookup(sym: &str) -> Option<Element> {
    let sym = sym.trim();
    Element::iter().find(|e| e.symbol().eq_ignore_ascii_case(sym))
}

datalink_extcore::declare! {
    core = Core;
    extension = "elements";
    version = env!("CARGO_PKG_VERSION");

    scalar element_name(text) -> text [propagate, deterministic] = |args| {
        let sym = args.arg_text(0, "element_name")?;
        Ok(match lookup(&sym) {
            Some(e) => NeutralValue::Text(e.name().to_string()),
            None => NeutralValue::Null,
        })
    };

    scalar element_number(text) -> int64 [propagate, deterministic] = |args| {
        let sym = args.arg_text(0, "element_number")?;
        Ok(match lookup(&sym) {
            Some(e) => NeutralValue::Int64(e.atomic_number() as i64),
            None => NeutralValue::Null,
        })
    };

    scalar element_weight(text) -> float64 [propagate, deterministic] = |args| {
        let sym = args.arg_text(0, "element_weight")?;
        Ok(match lookup(&sym) {
            Some(e) => NeutralValue::Float64(f64::from(e.atomic_weight())),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert_eq!(
            Core::dispatch(idx("element_name"), &[t("Fe")]).unwrap(),
            NeutralValue::Text(String::from("Iron"))
        );
        assert_eq!(
            Core::dispatch(idx("element_number"), &[t("O")]).unwrap(),
            NeutralValue::Int64(8)
        );
        match Core::dispatch(idx("element_weight"), &[t("H")]).unwrap() {
            NeutralValue::Float64(w) => assert!((w - 1.008).abs() < 1e-3, "got {w}"),
            other => panic!("expected float, got {other:?}"),
        }
        assert_eq!(
            Core::dispatch(idx("element_name"), &[t("Au")]).unwrap(),
            NeutralValue::Text(String::from("Gold"))
        );
        assert_eq!(
            Core::dispatch(idx("element_name"), &[t("Xx")]).unwrap(),
            NeutralValue::Null
        );
    }
}
