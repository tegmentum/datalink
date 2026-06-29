//! Neutral core for the `minijinja` extension — Jinja2-style templating
//! via the `minijinja` crate — written ONCE. The per-DB shim is generated
//! from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `jinja_render(template text, context_json text) -> text`
//!   * `jinja_valid(template text) -> boolean`
//!
//! `jinja_render` compiles `template` and renders it against the JSON object
//! in `context_json`; an invalid template, malformed JSON, or a render error
//! all yield NULL (never a panic). `jinja_valid` reports whether `template`
//! compiles. NULL inputs propagate to NULL (the `propagate` attribute).

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic templating logic.
pub mod logic {
    use minijinja::Environment;

    /// Compile + render `template` against the JSON value parsed from
    /// `context_json`. Returns `None` on bad JSON, a template compile
    /// error, or a render error.
    pub fn render(template: &str, context_json: &str) -> Option<std::string::String> {
        let ctx: serde_json::Value = serde_json::from_str(context_json).ok()?;
        let env = Environment::new();
        env.render_str(template, ctx).ok()
    }

    /// True iff `template` compiles as a minijinja template.
    pub fn valid(template: &str) -> bool {
        let mut env = Environment::new();
        env.add_template_owned("__validate__", template.to_string())
            .is_ok()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "minijinja";
    version = env!("CARGO_PKG_VERSION");

    scalar jinja_render(text, text) -> text [propagate, deterministic] = |args| {
        let template = args.arg_text(0, "jinja_render")?;
        let context = args.arg_text(1, "jinja_render")?;
        Ok(match logic::render(&template, &context) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar jinja_valid(text) -> boolean [propagate, deterministic] = |args| {
        let template = args.arg_text(0, "jinja_valid")?;
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
            Core::dispatch(idx("jinja_render"), &[t("Hello {{ name }}!"), t("{\"name\": \"World\"}")]).unwrap(),
            t("Hello World!")
        );
    }

    #[test]
    fn renders_loops_and_filters() {
        assert_eq!(
            Core::dispatch(
                idx("jinja_render"),
                &[t("{% for n in nums %}{{ n }}{% endfor %}"), t("{\"nums\": [1, 2, 3]}")]
            ).unwrap(),
            t("123")
        );
        assert_eq!(
            Core::dispatch(idx("jinja_render"), &[t("{{ word | upper }}"), t("{\"word\": \"hi\"}")]).unwrap(),
            t("HI")
        );
    }

    #[test]
    fn bad_template_renders_null() {
        assert_eq!(
            Core::dispatch(idx("jinja_render"), &[t("Hello {{ name "), t("{}")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn bad_json_renders_null() {
        assert_eq!(
            Core::dispatch(idx("jinja_render"), &[t("Hello {{ name }}"), t("not json")]).unwrap(),
            NeutralValue::Null
        );
    }

    #[test]
    fn valid_reports_compile_status() {
        assert_eq!(
            Core::dispatch(idx("jinja_valid"), &[t("Hello {{ name }}!")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("jinja_valid"), &[t("Hello {{ name ")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }
}
