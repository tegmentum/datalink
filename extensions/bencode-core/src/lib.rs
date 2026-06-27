//! Neutral core for the `bencode` extension — BitTorrent bencode via
//! `serde_bencode` — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `bencode_to_json(data blob) -> text`  (JSON; NULL on decode error)
//!   * `bencode_is_valid(data blob) -> boolean`
//!
//! Byte-strings decode to UTF-8 text where possible, else lowercase hex;
//! dict keys are canonically byte-sorted so output is deterministic.
//! Never panics. The surface is identical in both ports (zero drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use serde_bencode::value::Value as Ben;

/// Render bytes as a JSON string token: UTF-8 text when valid, else hex.
fn bytes_to_json_string(bytes: &[u8], out: &mut String) {
    match core::str::from_utf8(bytes) {
        Ok(s) => escape_json_string(s, out),
        Err(_) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                hex.push_str(&alloc::format!("{:02x}", b));
            }
            escape_json_string(&hex, out);
        }
    }
}

fn escape_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize a bencode value to JSON text. Dict keys are byte-strings
/// rendered the same way as scalar byte-strings (UTF-8 text or hex), and
/// sorted by raw byte key so JSON output is deterministic.
fn ben_to_json(v: &Ben, out: &mut String) {
    match v {
        Ben::Int(i) => out.push_str(&alloc::string::ToString::to_string(i)),
        Ben::Bytes(b) => bytes_to_json_string(b, out),
        Ben::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ben_to_json(item, out);
            }
            out.push(']');
        }
        Ben::Dict(map) => {
            let mut entries: alloc::vec::Vec<(&alloc::vec::Vec<u8>, &Ben)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (k, val)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                bytes_to_json_string(k, out);
                out.push(':');
                ben_to_json(val, out);
            }
            out.push('}');
        }
    }
}

/// Decode bencode bytes to JSON text, or None on a decode error.
pub fn decode_to_json(data: &[u8]) -> Option<String> {
    let v: Ben = serde_bencode::from_bytes(data).ok()?;
    let mut out = String::new();
    ben_to_json(&v, &mut out);
    Some(out)
}

/// True if `data` is valid bencode.
pub fn is_valid(data: &[u8]) -> bool {
    serde_bencode::from_bytes::<Ben>(data).is_ok()
}

datalink_extcore::declare! {
    core = Core;
    extension = "bencode";
    version = env!("CARGO_PKG_VERSION");

    scalar bencode_to_json(blob) -> text [propagate, deterministic] = |args| {
        let bytes = args.arg_blob(0, "bencode_to_json")?;
        Ok(match decode_to_json(&bytes) {
            Some(json) => NeutralValue::Text(json),
            None => NeutralValue::Null,
        })
    };
    // [called]: a NULL / non-blob argument yields `false` (not NULL),
    // matching the pre-pullup `bencode_is_valid`.
    scalar bencode_is_valid(blob) -> boolean [called, deterministic] = |args| {
        let bytes = match args.first() {
            Some(NeutralValue::Blob(b)) => b.clone(),
            Some(NeutralValue::Text(s)) => s.clone().into_bytes(),
            _ => return Ok(NeutralValue::Boolean(false)),
        };
        Ok(NeutralValue::Boolean(is_valid(&bytes)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }
    fn blob(b: &[u8]) -> NeutralValue { NeutralValue::Blob(b.to_vec()) }
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("bencode_to_json"), &[blob(b"d3:bar4:spam3:fooi42ee")]).unwrap(), t(r#"{"bar":"spam","foo":42}"#));
        assert_eq!(Core::dispatch(idx("bencode_to_json"), &[blob(b"l4:spam4:eggse")]).unwrap(), t(r#"["spam","eggs"]"#));
        assert_eq!(Core::dispatch(idx("bencode_to_json"), &[blob(b"i42e")]).unwrap(), t("42"));
        assert_eq!(Core::dispatch(idx("bencode_to_json"), &[blob(b"4:spam")]).unwrap(), t(r#""spam""#));
        assert_eq!(Core::dispatch(idx("bencode_is_valid"), &[blob(b"i42e")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("bencode_is_valid"), &[blob(b"i42")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("bencode_is_valid"), &[blob(b"xyz")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("bencode_to_json"), &[blob(b"not-bencode")]).unwrap(), NeutralValue::Null);
    }

    // Carried-over fuzz regressions (never-panic on untrusted bytes).
    #[test]
    fn decode_to_json_is_total() {
        assert_eq!(decode_to_json(b"i42e"), Some(String::from("42")));
        assert_eq!(decode_to_json(b""), None);
        assert_eq!(decode_to_json(b"i"), None);
        assert_eq!(decode_to_json(b"99999999999999999999:x"), None);
        assert_eq!(decode_to_json(b"l"), None);
        assert_eq!(decode_to_json(&[0xff, 0xfe, 0xfd]), None);
        assert_eq!(decode_to_json(b"2:\xff\xfe"), Some(String::from(r#""fffe""#)));
    }
}
