//! Neutral core for the `slug` extension — URL-safe slug generation (via the
//! `slug` crate) — written ONCE.
//!
//!   * `slugify(text) -> text`.

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::string::String;

    /// URL-safe slug ("Hello, World!" -> "hello-world").
    pub fn slugify(s: &str) -> String {
        slug::slugify(s)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "slug";
    version = env!("CARGO_PKG_VERSION");

    scalar slugify(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "slugify")?;
        Ok(NeutralValue::Text(logic::slugify(&s)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    #[test]
    fn slugifies() {
        let i = Core::DECLS.iter().position(|d| d.name == "slugify").unwrap();
        assert_eq!(
            Core::dispatch(i, &[NeutralValue::Text("Hello, World!".to_string())]).unwrap(),
            NeutralValue::Text("hello-world".to_string())
        );
    }
}
