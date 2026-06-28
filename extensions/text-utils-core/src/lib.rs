//! Neutral core for the `text-utils` extension — cross-dialect string
//! scalars, written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the functions a host does NOT already provide
//!
//! Of sqlink's 12 `text-utils` scalars, DuckDB already ships
//! `position`/`split_part`/`lcase`/`ucase`/`split`/`string_split`/
//! `str_split`/`reverse` as builtins (in `core_functions`/the default
//! macros), so re-registering them would collide with the builtin. Only
//! the genuinely-missing ones are declared here:
//!
//!   * `sql_normalize(sql) -> text`        — parameterize a SQL string
//!   * `insert(s, pos, len, repl) -> text` — MySQL `INSERT()`
//!   * `locate(substr, str) -> int64`      — MySQL `LOCATE()`, 1-based
//!   * `locate(substr, str, start) -> int64`
//!
//! The eponymous `prefixes` TABLE function from sqlink is intentionally
//! NOT pulled up: table functions are not codegen-able scalars (the
//! row-production shape is DB-specific) and stay hand-written per the
//! capability gradient. Its pure logic ([`logic::prefixes_of`]) is kept
//! here for the deferred DB-private vtab/replacement-scan shims.
//!
//! The logic is byte-identical to sqlink's `text-utils`, so a future
//! `sqlite_shim!` over this core reproduces sqlink's behaviour.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic text implementations. Native-testable.
pub mod logic {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    /// Replace SQL literals with `?` and collapse whitespace.
    /// Tokenizes via a tiny state machine that respects string-quote
    /// escaping (`''` inside a string is an escaped single quote, not a
    /// terminator); numbers and identifiers fold to a canonical form,
    /// keywords pass through lowercased.
    pub fn normalize_sql(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let chars: Vec<char> = sql.chars().collect();
        let mut i = 0usize;
        let mut last_was_ws = true; // suppress leading whitespace
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                if !last_was_ws {
                    out.push(' ');
                }
                last_was_ws = true;
                i += 1;
                continue;
            }
            last_was_ws = false;
            // String literal — scan to the closing quote, honoring
            // doubled-quote escapes ('it''s').
            if c == '\'' || c == '"' {
                let q = c;
                i += 1;
                while i < chars.len() {
                    if chars[i] == q {
                        if i + 1 < chars.len() && chars[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push('?');
                continue;
            }
            // Number — digits with optional decimal point and exponent.
            if c.is_ascii_digit() {
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                    i += 1;
                    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                        i += 1;
                    }
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                out.push('?');
                continue;
            }
            // Identifier or keyword — lowercase and emit.
            if c.is_ascii_alphabetic() || c == '_' {
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    out.push(chars[i].to_ascii_lowercase());
                    i += 1;
                }
                continue;
            }
            // Everything else (punctuation, parens, operators) unchanged.
            out.push(c);
            i += 1;
        }
        out.trim_end().to_string()
    }

    /// 1-based index of `needle` in `haystack`, counting CHARACTERS (not
    /// bytes), starting from 1-based `start`. 0 if not found.
    pub fn find_pos(haystack: &str, needle: &str, start: i64) -> i64 {
        if needle.is_empty() {
            return 0;
        }
        let chars: Vec<char> = haystack.chars().collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let from = (start.max(1) as usize).saturating_sub(1);
        if from >= chars.len() {
            return 0;
        }
        for i in from..=chars.len().saturating_sub(needle_chars.len()) {
            if chars[i..i + needle_chars.len()] == *needle_chars {
                return (i + 1) as i64;
            }
        }
        0
    }

    /// MySQL `INSERT(str, pos, len, newstr)` — replace `len` chars at
    /// 1-based `pos` with `newstr`. Out-of-range `pos` returns `str`.
    pub fn mysql_insert(s: &str, pos: i64, len: i64, repl: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        if pos < 1 || (pos as usize) > chars.len() {
            return s.to_string();
        }
        let start = (pos - 1) as usize;
        let end = (start + len.max(0) as usize).min(chars.len());
        let mut out: String = chars[..start].iter().collect();
        out.push_str(repl);
        out.extend(&chars[end..]);
        out
    }

    /// All non-empty prefixes of `s` (`"he" -> ["h", "he"]`). Kept for
    /// the deferred DB-private `prefixes` table function; not a declared
    /// scalar.
    pub fn prefixes_of(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::with_capacity(chars.len());
        for end in 1..=chars.len() {
            out.push(chars[..end].iter().collect());
        }
        out
    }
}

datalink_extcore::declare! {
    core = Core;
    // Underscore (not "text-utils"): the extension name doubles as the
    // unquoted DuckDB `LOAD <name>` identifier, where a hyphen is a parse
    // error. This matches sqlink's `preferred_prefix = "text_utils"`.
    extension = "text_utils";
    version = env!("CARGO_PKG_VERSION");

    scalar sql_normalize(text) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(logic::normalize_sql(&args.arg_text(0, "sql_normalize")?)))
    };

    // MySQL INSERT(str, pos, len, newstr).
    scalar insert(text, int64, int64, text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "insert")?;
        let pos = args.arg_int(1, "insert")?;
        let len = args.arg_int(2, "insert")?;
        let repl = args.arg_text(3, "insert")?;
        Ok(NeutralValue::Text(logic::mysql_insert(&s, pos, len, &repl)))
    };

    // MySQL LOCATE(substr, str) -> 1-based position, 0 if absent.
    scalar locate(text, text) -> int64 [propagate, deterministic] = |args| {
        let needle = args.arg_text(0, "locate")?;
        let hay = args.arg_text(1, "locate")?;
        Ok(NeutralValue::Int64(logic::find_pos(&hay, &needle, 1)))
    };

    // MySQL LOCATE(substr, str, start) -> 1-based position from `start`.
    scalar locate(text, text, int64) -> int64 [propagate, deterministic] = |args| {
        let needle = args.arg_text(0, "locate")?;
        let hay = args.arg_text(1, "locate")?;
        let start = args.arg_int(2, "locate")?;
        Ok(NeutralValue::Int64(logic::find_pos(&hay, &needle, start)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx_arity(n: &str, arity: usize) -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == n && d.args.len() == arity)
            .unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(alloc::string::String::from(s))
    }

    #[test]
    fn normalize() {
        assert_eq!(
            Core::dispatch(idx_arity("sql_normalize", 1), &[t("SELECT * FROM t WHERE name='alice' AND age=30")]).unwrap(),
            t("select * from t where name=? and age=?")
        );
    }

    #[test]
    fn mysql_insert() {
        assert_eq!(
            Core::dispatch(
                idx_arity("insert", 4),
                &[t("Quadratic"), NeutralValue::Int64(3), NeutralValue::Int64(4), t("What")]
            )
            .unwrap(),
            t("QuWhattic")
        );
    }

    #[test]
    fn locate() {
        assert_eq!(
            Core::dispatch(idx_arity("locate", 2), &[t("bar"), t("foobarbar")]).unwrap(),
            NeutralValue::Int64(4)
        );
        assert_eq!(
            Core::dispatch(idx_arity("locate", 3), &[t("bar"), t("foobarbar"), NeutralValue::Int64(5)]).unwrap(),
            NeutralValue::Int64(7)
        );
        assert_eq!(
            Core::dispatch(idx_arity("locate", 2), &[t("zzz"), t("foobarbar")]).unwrap(),
            NeutralValue::Int64(0)
        );
    }
}
