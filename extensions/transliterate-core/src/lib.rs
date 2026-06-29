//! Neutral core for the `transliterate` extension — Unicode -> ASCII
//! transliteration via the `deunicode` crate — written ONCE. The per-DB
//! shims (ducklink `duckdb:extension`, sqlink `sqlite:extension`) are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `deunicode(text) -> text` — e.g. "Æneid café" -> "AEneid cafe",
//!     "北京" -> "Bei Jing ". `NULL -> NULL`.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "transliterate";
    version = env!("CARGO_PKG_VERSION");

    scalar deunicode(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "deunicode")?;
        Ok(NeutralValue::Text(deunicode::deunicode(&s)))
    };
}
