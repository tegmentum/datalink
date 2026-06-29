//! Neutral core for the `tera` extension — Jinja2/Django-style templating
//! via the `tera` crate — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table. Direct twin of the
//! `minijinja` core.
//!
//!   * `tera_render(template text, context_json text) -> text`
//!   * `tera_valid(template text) -> boolean`
//!
//! `tera_render` compiles `template` and renders it against the JSON object
//! in `context_json`; an invalid template, malformed/non-object JSON, or a
//! render error all yield NULL (never a panic). `tera_valid` reports whether
//! `template` compiles. NULL inputs propagate to NULL (the `propagate`
//! attribute).

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic templating logic.
pub mod logic {
    use tera::{Context, Tera};

    /// Compile + render `template` against the JSON value parsed from
    /// `context_json`. Returns `None` on bad/non-object JSON, a template
    /// compile error, or a render error.
    pub fn render(template: &str, context_json: &str) -> Option<std::string::String> {
        let val: serde_json::Value = serde_json::from_str(context_json).ok()?;
        let ctx = Context::from_value(val).ok()?;
        Tera::one_off(template, &ctx, false).ok()
    }

    /// True iff `template` compiles as a tera template.
    pub fn valid(template: &str) -> bool {
        let mut tera = Tera::default();
        tera.add_raw_template("__validate__", template).is_ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "tera";
    version = env!("CARGO_PKG_VERSION");

    scalar tera_render(text, text) -> text [propagate, deterministic] = |args| {
        let template = args.arg_text(0, "tera_render")?;
        let context = args.arg_text(1, "tera_render")?;
        Ok(match logic::render(&template, &context) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar tera_valid(text) -> boolean [propagate, deterministic] = |args| {
        let template = args.arg_text(0, "tera_valid")?;
        Ok(NeutralValue::Boolean(logic::valid(&template)))
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
    fn renders_with_context() {
        assert_eq!(
            Core::dispatch(idx("tera_render"), &[t("Hello {{ name }}!"), t("{\"name\": \"World\"}")]).unwrap(),
            t("Hello World!")
        );
    }

    #[test]
    fn renders_loops_and_filters() {
        assert_eq!(
            Core::dispatch(
                idx("tera_render"),
                &[t("{% for n in nums %}{{ n }}{% endfor %}"), t("{\"nums\": [1, 2, 3]}")]
            ).unwrap(),
            t("123")
        );
        assert_eq!(
            Core::dispatch(idx("tera_render"), &[t("{{ word | upper }}"), t("{\"word\": \"hi\"}")]).unwrap(),
            t("HI")
        );
    }

    #[test]
    fn bad_template_renders_null() {
        assert_eq!(
            Core::dispatch(idx("tera_render"), &[t("Hello {{ name "), t("{}")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn bad_json_renders_null() {
        assert_eq!(
            Core::dispatch(idx("tera_render"), &[t("Hello {{ name }}"), t("not json")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn valid_reports_compile_status() {
        assert_eq!(
            Core::dispatch(idx("tera_valid"), &[t("Hello {{ name }}!")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("tera_valid"), &[t("Hello {{ name ")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }
}
