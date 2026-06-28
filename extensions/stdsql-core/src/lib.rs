//! Neutral core for the `stdsql` cross-dialect scalar pack — the
//! portability layer that surfaces the function spellings other engines
//! ship, written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the names DuckDB does NOT ship as builtins
//!
//! `stdsql` originated in sqlink because SQLite ships almost none of the
//! cross-DB standard-SQL scalars. DuckDB is much richer: it already
//! provides `greatest`/`least` (variadic), `left`/`right`, `lpad`/`rpad`
//! (3-arg), `repeat`, `starts_with`/`ends_with`, `translate`, `to_hex`,
//! `bit_length`, `chr`, `ascii`, `char_length`/`character_length`,
//! `from_hex`, `get_bit`/`set_bit` as BUILTINS. Re-registering any of
//! those (same name + arity) would collide with the builtin, so they are
//! deliberately NOT declared here. `if` is a reserved CASE-syntax keyword
//! in DuckDB (`if(c,a,b)` parses to `CASE WHEN c THEN a ELSE b END`), so
//! it is also skipped — a registered `if` function would be unreachable.
//! `lpad`/`rpad` are skipped entirely (the 2-arg gap would be an overload
//! ON a builtin name — outside the safe no-builtin-overlap envelope).
//!
//! What remains — and is what ducklink GAINS — are the spellings DuckDB
//! has no builtin for: `space`, `initcap`; the ClickHouse camelCase
//! family (`startsWith`/`endsWith`/`lengthUTF8`/`lowerUTF8`/`upperUTF8`/
//! `toString`/`empty`/`notEmpty`/`replaceAll`/`positionUTF8`); and the
//! PostgreSQL `to_bin`/`to_oct`/`to_ascii`/`quote_ident`/`quote_literal`/
//! `quote_nullable`/`get_byte`/`set_byte`. The algorithms are
//! byte-identical to sqlink's `stdsql`, so a future `sqlite_shim!` over
//! this core reproduces sqlink's behaviour.
//!
//! NOTE on `quote_nullable`: DuckDB propagates NULL by default (a NULL
//! argument yields NULL WITHOUT invoking the function), so the
//! "NULL -> 'NULL'" branch is unreachable on the DuckDB host; the
//! non-NULL path matches sqlink exactly. Declared `propagate` accordingly.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic string algorithms, lifted byte-for-byte from sqlink's
/// `stdsql` extension. Native-testable; the generated shim is a thin
/// wrapper.
pub mod algo {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn space(n: i64) -> String {
        if n <= 0 {
            return String::new();
        }
        " ".repeat(n as usize)
    }

    pub fn starts_with(s: &str, prefix: &str) -> bool {
        s.starts_with(prefix)
    }

    pub fn ends_with(s: &str, suffix: &str) -> bool {
        s.ends_with(suffix)
    }

    pub fn initcap(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut word_start = true;
        for c in s.chars() {
            if c.is_whitespace() || c == '_' || c == '-' {
                word_start = true;
                out.push(c);
            } else if word_start {
                out.extend(c.to_uppercase());
                word_start = false;
            } else {
                out.extend(c.to_lowercase());
            }
        }
        out
    }

    pub fn char_length(s: &str) -> i64 {
        s.chars().count() as i64
    }

    /// 1-based char index of the first occurrence of `needle` in `s`, or
    /// 0 if not found (ClickHouse `positionUTF8`). Char-based, not byte.
    pub fn position_utf8(s: &str, needle: &str) -> i64 {
        let chars: Vec<char> = s.chars().collect();
        let nchars: Vec<char> = needle.chars().collect();
        if nchars.is_empty() {
            return 0;
        }
        if nchars.len() > chars.len() {
            return 0;
        }
        for i in 0..=chars.len() - nchars.len() {
            if chars[i..i + nchars.len()] == *nchars {
                return (i + 1) as i64;
            }
        }
        0
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "stdsql";
    version = env!("CARGO_PKG_VERSION");

    // MySQL/MariaDB SPACE(n) -> n spaces.
    scalar space(int64) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(algo::space(a.arg_int(0, "space")?)))
    };

    // PostgreSQL INITCAP -> title-case (word boundaries on whitespace/_/-).
    scalar initcap(text) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(algo::initcap(&a.arg_text(0, "initcap")?)))
    };

    // ---- ClickHouse camelCase family (DuckDB has the snake_case
    //      canonical forms as builtins; these spellings are gaps) ----

    // startsWith / endsWith -> 0/1 (matching sqlink's Integer result).
    scalar startsWith(text, text) -> int64 [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "startsWith")?;
        let p = a.arg_text(1, "startsWith")?;
        Ok(NeutralValue::Int64(algo::starts_with(&s, &p) as i64))
    };
    scalar endsWith(text, text) -> int64 [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "endsWith")?;
        let p = a.arg_text(1, "endsWith")?;
        Ok(NeutralValue::Int64(algo::ends_with(&s, &p) as i64))
    };
    // lengthUTF8 counts characters not bytes -> char_length.
    scalar lengthUTF8(text) -> int64 [propagate, deterministic] = |a| {
        Ok(NeutralValue::Int64(algo::char_length(&a.arg_text(0, "lengthUTF8")?)))
    };
    scalar lowerUTF8(text) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(a.arg_text(0, "lowerUTF8")?.to_lowercase()))
    };
    scalar upperUTF8(text) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(a.arg_text(0, "upperUTF8")?.to_uppercase()))
    };
    // toString(x) -> SQL-side cast-to-text (best-effort over the TEXT arg).
    scalar toString(text) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(a.arg_text(0, "toString")?))
    };
    scalar empty(text) -> int64 [propagate, deterministic] = |a| {
        Ok(NeutralValue::Int64(a.arg_text(0, "empty")?.is_empty() as i64))
    };
    scalar notEmpty(text) -> int64 [propagate, deterministic] = |a| {
        Ok(NeutralValue::Int64((!a.arg_text(0, "notEmpty")?.is_empty()) as i64))
    };
    scalar replaceAll(text, text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "replaceAll")?;
        let from = a.arg_text(1, "replaceAll")?;
        let to = a.arg_text(2, "replaceAll")?;
        Ok(NeutralValue::Text(s.replace(from.as_str(), &to)))
    };
    scalar positionUTF8(text, text) -> int64 [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "positionUTF8")?;
        let n = a.arg_text(1, "positionUTF8")?;
        Ok(NeutralValue::Int64(algo::position_utf8(&s, &n)))
    };

    // ---- PostgreSQL to_* / quote_* / byte accessors ----

    scalar to_bin(int64) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(::alloc::format!("{:b}", a.arg_int(0, "to_bin")? as u64)))
    };
    scalar to_oct(int64) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(::alloc::format!("{:o}", a.arg_int(0, "to_oct")? as u64)))
    };
    // PG to_ascii: best-effort -> drop non-ASCII so output is pure ASCII.
    scalar to_ascii(text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "to_ascii")?;
        Ok(NeutralValue::Text(s.chars().filter(|c| c.is_ascii()).collect()))
    };
    scalar quote_ident(text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "quote_ident")?;
        Ok(NeutralValue::Text(::alloc::format!("\"{}\"", s.replace('"', "\"\""))))
    };
    scalar quote_literal(text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "quote_literal")?;
        Ok(NeutralValue::Text(::alloc::format!("'{}'", s.replace('\'', "''"))))
    };
    // quote_nullable on a non-NULL arg == quote_literal. The NULL -> 'NULL'
    // branch is unreachable under DuckDB's default NULL propagation.
    scalar quote_nullable(text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "quote_nullable")?;
        Ok(NeutralValue::Text(::alloc::format!("'{}'", s.replace('\'', "''"))))
    };
    scalar get_byte(int64, int64) -> int64 [propagate, deterministic] = |a| {
        let n = a.arg_int(0, "get_byte")?;
        let i = a.arg_int(1, "get_byte")?;
        Ok(NeutralValue::Int64((((n as u64) >> (i * 8)) & 0xff) as i64))
    };
    scalar set_byte(int64, int64, int64) -> int64 [propagate, deterministic] = |a| {
        let n = a.arg_int(0, "set_byte")?;
        let i = a.arg_int(1, "set_byte")?;
        let v = a.arg_int(2, "set_byte")?;
        let shift = i * 8;
        let mask = 0xffu64 << shift;
        let result = ((n as u64) & !mask) | (((v as u64) & 0xff) << shift);
        Ok(NeutralValue::Int64(result as i64))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(n: &str, arity: usize) -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == n && d.args.len() == arity)
            .unwrap_or_else(|| panic!("no decl {n}/{arity}"))
    }
    fn call(n: &str, arity: usize, args: &[NeutralValue]) -> NeutralValue {
        Core::dispatch(idx(n, arity), args).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(s.into())
    }
    fn i(n: i64) -> NeutralValue {
        NeutralValue::Int64(n)
    }

    #[test]
    fn strings() {
        assert_eq!(call("space", 1, &[i(3)]), t("   "));
        assert_eq!(call("initcap", 1, &[t("hello world")]), t("Hello World"));
        assert_eq!(call("toString", 1, &[t("42")]), t("42"));
        assert_eq!(call("lowerUTF8", 1, &[t("ABC")]), t("abc"));
        assert_eq!(call("upperUTF8", 1, &[t("abc")]), t("ABC"));
        assert_eq!(call("replaceAll", 3, &[t("a.b.c"), t("."), t("-")]), t("a-b-c"));
    }

    #[test]
    fn predicates() {
        assert_eq!(call("startsWith", 2, &[t("foobar"), t("foo")]), i(1));
        assert_eq!(call("endsWith", 2, &[t("foobar"), t("baz")]), i(0));
        assert_eq!(call("empty", 1, &[t("")]), i(1));
        assert_eq!(call("notEmpty", 1, &[t("x")]), i(1));
        assert_eq!(call("lengthUTF8", 1, &[t("héllo")]), i(5));
        assert_eq!(call("positionUTF8", 2, &[t("abcdef"), t("cd")]), i(3));
        assert_eq!(call("positionUTF8", 2, &[t("abc"), t("z")]), i(0));
    }

    #[test]
    fn pg_radix_and_quote() {
        assert_eq!(call("to_bin", 1, &[i(5)]), t("101"));
        assert_eq!(call("to_oct", 1, &[i(8)]), t("10"));
        assert_eq!(call("to_ascii", 1, &[t("café")]), t("caf"));
        assert_eq!(call("quote_ident", 1, &[t("a\"b")]), t("\"a\"\"b\""));
        assert_eq!(call("quote_literal", 1, &[t("it's")]), t("'it''s'"));
        assert_eq!(call("quote_nullable", 1, &[t("x")]), t("'x'"));
    }

    #[test]
    fn byte_accessors() {
        // 0x01020304: byte 0 = 0x04 = 4, byte 1 = 0x03 = 3.
        assert_eq!(call("get_byte", 2, &[i(0x01020304), i(0)]), i(4));
        assert_eq!(call("get_byte", 2, &[i(0x01020304), i(1)]), i(3));
        // set byte 0 to 0xff -> 0x010203ff.
        assert_eq!(call("set_byte", 3, &[i(0x01020304), i(0), i(0xff)]), i(0x010203ff));
    }
}
