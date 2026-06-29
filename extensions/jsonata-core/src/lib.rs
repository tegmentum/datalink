//! Neutral core for the `jsonata` extension — the JSONata query and
//! transformation language via the `jsonata-core` crate — written ONCE. The
//! per-DB shim is generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `jsonata(expr text, json text) -> text`
//!
//! `jsonata` compiles `expr`, evaluates it against the document parsed from
//! `json`, and renders the result back to a JSON string. An invalid
//! expression, malformed input JSON, or an evaluation error all yield NULL
//! (never a panic). NULL inputs propagate to NULL.
//!
//! The package is named `jsonata-core-ext` (lib `jsonata_core_ext`) so it does
//! not collide with the upstream `jsonata-core` dependency it wraps.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic JSONata logic.
pub mod logic {
    use jsonata_core::evaluator::Evaluator;
    use jsonata_core::parser;
    use jsonata_core::value::JValue;

    /// Evaluate `expr` against `json` and render the result as a JSON string.
    /// Returns `None` on a parse error, malformed JSON, an evaluation error,
    /// or a serialization error.
    pub fn eval(expr: &str, json: &str) -> Option<std::string::String> {
        let ast = parser::parse(expr).ok()?;
        let data = JValue::from_json_str(json).ok()?;
        let result = Evaluator::new().evaluate(&ast, &data).ok()?;
        result.to_json_string().ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "jsonata";
    version = env!("CARGO_PKG_VERSION");

    scalar jsonata(text, text) -> text [propagate, deterministic] = |args| {
        let expr = args.arg_text(0, "jsonata")?;
        let json = args.arg_text(1, "jsonata")?;
        Ok(match logic::eval(&expr, &json) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(alloc::string::String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn path_navigation() {
        assert_eq!(
            Core::dispatch(idx("jsonata"), &[t("a.b"), t("{\"a\":{\"b\":42}}")]).unwrap(),
            t("42")
        );
    }

    #[test]
    fn aggregate_and_filter() {
        assert_eq!(
            Core::dispatch(
                idx("jsonata"),
                &[t("$sum(items.price)"), t("{\"items\":[{\"price\":10},{\"price\":20}]}")]
            ).unwrap(),
            t("30")
        );
        assert_eq!(
            Core::dispatch(
                idx("jsonata"),
                &[t("orders[price > 100].product"),
                  t("{\"orders\":[{\"product\":\"Laptop\",\"price\":1200},{\"product\":\"Mouse\",\"price\":25}]}")]
            ).unwrap(),
            t("\"Laptop\"")
        );
    }

    #[test]
    fn bad_expr_is_null() {
        assert_eq!(
            Core::dispatch(idx("jsonata"), &[t("bad syntax ("), t("{}")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn bad_json_is_null() {
        assert_eq!(
            Core::dispatch(idx("jsonata"), &[t("a"), t("not json")]).unwrap(),
            NeutralValue::Null
        );
    }
}
