//! Neutral core for the `textdiff` extension — text diffing via the `similar`
//! crate — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `text_diff(a, b) -> text`           unified line-based diff ("" if equal)
//!   * `diff_ratio(a, b) -> float64`       character-level similarity in [0,1]
//!   * `diff_changed_lines(a, b) -> int64` count of inserted + deleted lines
//!
//! `NULL` on any NULL input. Never panics.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use similar::{ChangeTag, TextDiff};

    /// A standard line-based unified diff of `a` -> `b`. Empty string if identical.
    pub fn unified(a: &str, b: &str) -> String {
        if a == b {
            return String::new();
        }
        let diff = TextDiff::from_lines(a, b);
        let mut out = String::new();
        for group in diff.grouped_ops(3) {
            for op in group {
                for change in diff.iter_changes(&op) {
                    let sign = match change.tag() {
                        ChangeTag::Delete => "-",
                        ChangeTag::Insert => "+",
                        ChangeTag::Equal => " ",
                    };
                    out.push_str(sign);
                    out.push_str(change.value());
                    if !change.value().ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    /// Character-granularity ratio (difflib SequenceMatcher.ratio() over chars).
    pub fn ratio(a: &str, b: &str) -> f64 {
        TextDiff::from_chars(a, b).ratio() as f64
    }

    pub fn changed_lines(a: &str, b: &str) -> i64 {
        let diff = TextDiff::from_lines(a, b);
        diff.iter_all_changes()
            .filter(|c| matches!(c.tag(), ChangeTag::Delete | ChangeTag::Insert))
            .count() as i64
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "textdiff";
    version = env!("CARGO_PKG_VERSION");

    scalar text_diff(text, text) -> text [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "text_diff")?;
        let b = args.arg_text(1, "text_diff")?;
        Ok(NeutralValue::Text(logic::unified(&a, &b)))
    };

    scalar diff_ratio(text, text) -> float64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "diff_ratio")?;
        let b = args.arg_text(1, "diff_ratio")?;
        Ok(NeutralValue::Float64(logic::ratio(&a, &b)))
    };

    scalar diff_changed_lines(text, text) -> int64 [propagate, deterministic] = |args| {
        let a = args.arg_text(0, "diff_changed_lines")?;
        let b = args.arg_text(1, "diff_changed_lines")?;
        Ok(NeutralValue::Int64(logic::changed_lines(&a, &b)))
    };
}
