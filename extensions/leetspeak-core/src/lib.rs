//! Neutral core for the `leetspeak` extension — the `to_leet` text
//! transform — written ONCE. The per-DB shims (ducklink
//! `duckdb:extension`, sqlink `sqlite:extension`, sqlink embed) are
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `to_leet(text) -> text` — a->4, e->3, i->1, o->0, s->5, t->7,
//!     b->8, g->6, l->1; other characters unchanged. NULL -> NULL.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    pub fn to_leet(s: &str) -> String {
        s.chars()
            .map(|c| match c.to_ascii_lowercase() {
                'a' => '4',
                'e' => '3',
                'i' => '1',
                'o' => '0',
                's' => '5',
                't' => '7',
                'b' => '8',
                'g' => '6',
                'l' => '1',
                _ => c,
            })
            .collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "leetspeak";
    version = env!("CARGO_PKG_VERSION");

    scalar to_leet(text) -> text [propagate, deterministic] = |args| {
        let s: String = args.arg_text(0, "to_leet")?;
        Ok(NeutralValue::Text(logic::to_leet(&s)))
    };
}
