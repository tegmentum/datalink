//! Neutral core for the `magic` extension — file content-type sniffing
//! from magic bytes via the pure-Rust `infer` crate — written ONCE. The
//! per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `magic_mime(data) -> text`          (e.g. 'image/png')
//!   * `magic_extension(data) -> text`     (e.g. 'png')
//!   * `magic_matcher_type(data) -> text`  (image/video/audio/...)
//!   * `is_image(data) -> boolean`
//!
//! Unknown type -> NULL (the sniffers); `is_image` is total on bytes.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Logic helper: infer matcher class -> stable string (DB-agnostic).
pub mod logic {
    pub fn matcher_type_str(m: infer::MatcherType) -> &'static str {
        use infer::MatcherType::*;
        match m {
            App => "app",
            Archive => "archive",
            Audio => "audio",
            Book => "book",
            Doc => "doc",
            Font => "font",
            Image => "image",
            Text => "text",
            Video => "video",
            Custom => "custom",
        }
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "magic";
    version = env!("CARGO_PKG_VERSION");

    scalar magic_mime(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "magic_mime")?;
        Ok(match infer::get(&b) {
            Some(k) => NeutralValue::Text(String::from(k.mime_type())),
            None => NeutralValue::Null,
        })
    };

    scalar magic_extension(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "magic_extension")?;
        Ok(match infer::get(&b) {
            Some(k) => NeutralValue::Text(String::from(k.extension())),
            None => NeutralValue::Null,
        })
    };

    scalar magic_matcher_type(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "magic_matcher_type")?;
        Ok(match infer::get(&b) {
            Some(k) => NeutralValue::Text(String::from(logic::matcher_type_str(k.matcher_type()))),
            None => NeutralValue::Null,
        })
    };

    scalar is_image(blob) -> boolean [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "is_image")?;
        Ok(NeutralValue::Boolean(infer::is_image(&b)))
    };
}
