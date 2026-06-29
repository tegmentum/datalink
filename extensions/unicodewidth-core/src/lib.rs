//! Neutral core for the `unicodewidth` extension — grapheme + display-width
//! measures — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `grapheme_count(text) -> int64`  user-perceived characters
//!     (unicode-segmentation).
//!   * `display_width(text) -> int64`  terminal columns (unicode-width; wide
//!     CJK = 2). `NULL -> NULL`.

extern crate alloc;

use datalink_extcore::NeutralValue;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

datalink_extcore::declare! {
    core = Core;
    extension = "unicodewidth";
    version = env!("CARGO_PKG_VERSION");

    scalar grapheme_count(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "grapheme_count")?;
        Ok(NeutralValue::Int64(s.graphemes(true).count() as i64))
    };

    scalar display_width(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "display_width")?;
        Ok(NeutralValue::Int64(UnicodeWidthStr::width(s.as_str()) as i64))
    };
}
