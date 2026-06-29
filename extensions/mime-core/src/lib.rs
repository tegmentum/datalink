//! Neutral core for the `mime` extension — filename/extension -> MIME
//! type via the `mime_guess` crate — written ONCE. The per-DB shims are
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `mime_type(path) -> text` — filename/path -> MIME type.
//!   * `mime_from_ext(ext) -> text` — extension -> MIME type.
//!
//! Unknown extension -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "mime";
    version = env!("CARGO_PKG_VERSION");

    scalar mime_type(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "mime_type")?;
        Ok(match mime_guess::from_path(s.as_str()).first_raw() {
            Some(m) => NeutralValue::Text(String::from(m)),
            None => NeutralValue::Null,
        })
    };

    scalar mime_from_ext(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "mime_from_ext")?;
        Ok(match mime_guess::from_ext(s.trim_start_matches('.')).first_raw() {
            Some(m) => NeutralValue::Text(String::from(m)),
            None => NeutralValue::Null,
        })
    };
}
