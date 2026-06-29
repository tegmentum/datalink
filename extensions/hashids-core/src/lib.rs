//! Neutral core for the `hashids` extension — Hashids integer-id
//! obfuscation via `harsh` — written ONCE. The per-DB shims are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `hashids_encode(number bigint, salt text) -> text` — YouTube-style id.
//!   * `hashids_decode(text, salt text) -> bigint` — the first decoded value.
//!
//! NULL number / undecodable -> NULL; a NULL salt coerces to the empty salt
//! (matching the pre-pullup `unwrap_or_default()`). Negative numbers -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::{ArgExt, NeutralValue};
use harsh::Harsh;

fn harsh_with(salt: &str) -> Option<Harsh> {
    Harsh::builder().salt(salt).build().ok()
}

pub fn encode(n: u64, salt: &str) -> Option<String> {
    let h = harsh_with(salt)?;
    let s = h.encode(&[n]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn decode(input: &str, salt: &str) -> Option<i64> {
    let h = harsh_with(salt)?;
    h.decode(input.trim())
        .ok()
        .and_then(|v| v.first().copied())
        .map(|n| n as i64)
}

datalink_extcore::declare! {
    core = Core;
    extension = "hashids";
    version = env!("CARGO_PKG_VERSION");

    scalar hashids_encode(int64, text) -> text [called, deterministic] = |args| {
        // Number arg: NULL / negative -> NULL.
        let n = match args.first() {
            Some(NeutralValue::Int64(n)) if *n >= 0 => *n as u64,
            _ => return Ok(NeutralValue::Null),
        };
        let salt = args.arg_text(1, "hashids_encode")?; // NULL salt -> "".
        Ok(match encode(n, &salt) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar hashids_decode(text, text) -> int64 [called, deterministic] = |args| {
        // A genuine NULL input -> NULL (do not coerce to "").
        let input = match args.first() {
            Some(NeutralValue::Text(s)) => s.clone(),
            _ => return Ok(NeutralValue::Null),
        };
        let salt = args.arg_text(1, "hashids_decode")?;
        Ok(match decode(&input, &salt) {
            Some(n) => NeutralValue::Int64(n),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn roundtrip() {
        let enc = match Core::dispatch(idx("hashids_encode"), &[NeutralValue::Int64(42), NeutralValue::Text(String::from("salt"))]).unwrap() {
            NeutralValue::Text(s) => s,
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(
            Core::dispatch(idx("hashids_decode"), &[NeutralValue::Text(enc), NeutralValue::Text(String::from("salt"))]).unwrap(),
            NeutralValue::Int64(42)
        );
    }

    #[test]
    fn negative_is_null() {
        assert_eq!(
            Core::dispatch(idx("hashids_encode"), &[NeutralValue::Int64(-1), NeutralValue::Text(String::from("s"))]).unwrap(),
            NeutralValue::Null
        );
    }
}
