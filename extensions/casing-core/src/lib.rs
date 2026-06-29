//! Neutral core for the `casing` extension — identifier case conversions
//! (via the `heck` crate) — written ONCE. The per-DB shims are generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `to_snake_case(text) -> text`
//!   * `to_camel_case(text) -> text`    (lowerCamelCase)
//!   * `to_pascal_case(text) -> text`   (UpperCamelCase)
//!   * `to_kebab_case(text) -> text`
//!   * `to_title_case(text) -> text`
//!   * `to_constant_case(text) -> text` (SHOUTY_SNAKE_CASE)
//!
//! Not `#![no_std]`: the `heck` crate is consumed with `std`.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use heck::{
        ToKebabCase, ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToTitleCase,
        ToUpperCamelCase,
    };

    pub fn snake(s: &str) -> String {
        s.to_snake_case()
    }
    pub fn camel(s: &str) -> String {
        s.to_lower_camel_case()
    }
    pub fn pascal(s: &str) -> String {
        s.to_upper_camel_case()
    }
    pub fn kebab(s: &str) -> String {
        s.to_kebab_case()
    }
    pub fn title(s: &str) -> String {
        s.to_title_case()
    }
    pub fn constant(s: &str) -> String {
        s.to_shouty_snake_case()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "casing";
    version = env!("CARGO_PKG_VERSION");

    scalar to_snake_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::snake(&args.arg_text(0, "to_snake_case")?)))
    };
    scalar to_camel_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::camel(&args.arg_text(0, "to_camel_case")?)))
    };
    scalar to_pascal_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::pascal(&args.arg_text(0, "to_pascal_case")?)))
    };
    scalar to_kebab_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::kebab(&args.arg_text(0, "to_kebab_case")?)))
    };
    scalar to_title_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::title(&args.arg_text(0, "to_title_case")?)))
    };
    scalar to_constant_case(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::constant(&args.arg_text(0, "to_constant_case")?)))
    };
}
