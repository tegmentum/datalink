//! Neutral core for the `hexdump` extension — classic hex dumps of bytes —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `hexdump(data blob) -> text`    — canonical `xxd` / `hexdump -C` dump.
//!   * `hex_pretty(data blob) -> text` — space-separated lowercase hex bytes.
//!
//! NULL input -> NULL (propagate). A TEXT argument is hashed as its UTF-8
//! bytes (matching the pre-pullup `blob_arg`). Never panics. Identical in
//! both ports.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use datalink_extcore::{ArgExt, NeutralValue};

/// `xxd` / `hexdump -C`: OFFSET(8 hex)  16 hex bytes (8|8 grouped)  |ascii|.
pub fn canonical_dump(data: &[u8]) -> String {
    let mut out = String::new();
    for (line, chunk) in data.chunks(16).enumerate() {
        let offset = line * 16;
        out.push_str(&format!("{:08x}  ", offset));
        for col in 0..16 {
            if col == 8 {
                out.push(' ');
            }
            match chunk.get(col) {
                Some(b) => out.push_str(&format!("{:02x} ", b)),
                None => out.push_str("   "),
            }
        }
        out.push('|');
        for &b in chunk {
            out.push(if (0x20..=0x7e).contains(&b) { b as char } else { '.' });
        }
        out.push('|');
        out.push('\n');
    }
    out
}

/// Space-separated lowercase hex bytes: "de ad be ef".
pub fn pretty_hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", b));
    }
    out
}

datalink_extcore::declare! {
    core = Core;
    extension = "hexdump";
    version = env!("CARGO_PKG_VERSION");

    scalar hexdump(blob) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(canonical_dump(&args.arg_blob(0, "hexdump")?)))
    };

    scalar hex_pretty(blob) -> text [propagate, deterministic] = |args| {
        Ok(NeutralValue::Text(pretty_hex(&args.arg_blob(0, "hex_pretty")?)))
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
    fn pretty_and_dump() {
        let b = NeutralValue::Blob(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            Core::dispatch(idx("hex_pretty"), &[b.clone()]).unwrap(),
            NeutralValue::Text(String::from("de ad be ef"))
        );
        let d = match Core::dispatch(idx("hexdump"), &[b]).unwrap() {
            NeutralValue::Text(s) => s,
            other => panic!("{other:?}"),
        };
        assert!(d.starts_with("00000000  de ad be ef "));
        assert!(d.contains("|...."));
    }
}
