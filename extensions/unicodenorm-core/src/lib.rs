//! Neutral core for the `unicodenorm` extension — Unicode normalization —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `nfc / nfd / nfkc / nfkd(text) -> text`
//!
//! Useful for dedup/compare of visually-identical strings with different
//! code-point sequences. `NULL -> NULL`.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;
use unicode_normalization::UnicodeNormalization;

datalink_extcore::declare! {
    core = Core;
    extension = "unicodenorm";
    version = env!("CARGO_PKG_VERSION");

    scalar nfc(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nfc")?;
        let out: String = s.nfc().collect();
        Ok(NeutralValue::Text(out))
    };

    scalar nfd(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nfd")?;
        let out: String = s.nfd().collect();
        Ok(NeutralValue::Text(out))
    };

    scalar nfkc(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nfkc")?;
        let out: String = s.nfkc().collect();
        Ok(NeutralValue::Text(out))
    };

    scalar nfkd(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "nfkd")?;
        let out: String = s.nfkd().collect();
        Ok(NeutralValue::Text(out))
    };
}
