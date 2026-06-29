//! Neutral core for the `braille` extension — text -> Unicode Braille
//! (grade-1 English letters; a-z map to the U+2800 Braille Patterns block,
//! case-folded; everything else passes through) — written ONCE. The per-DB
//! shims are generated from the [`declare!`](datalink_extcore::declare) table
//! below.
//!
//!   * `to_braille(text) -> text`

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;

    /// a-z in standard English Braille (Unicode Braille Patterns U+2800 block).
    const LETTERS: [char; 26] = [
        '\u{2801}', '\u{2803}', '\u{2809}', '\u{2819}', '\u{2811}', '\u{280B}', '\u{281B}',
        '\u{2813}', '\u{280A}', '\u{281A}', '\u{2805}', '\u{2807}', '\u{280D}', '\u{281D}',
        '\u{2815}', '\u{280F}', '\u{281F}', '\u{2817}', '\u{280E}', '\u{281E}', '\u{2825}',
        '\u{2827}', '\u{283A}', '\u{282D}', '\u{283D}', '\u{2835}',
    ];

    pub fn to_braille(s: &str) -> String {
        s.chars()
            .map(|c| {
                let lc = c.to_ascii_lowercase();
                if lc.is_ascii_lowercase() {
                    LETTERS[(lc as u8 - b'a') as usize]
                } else {
                    c
                }
            })
            .collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "braille";
    version = env!("CARGO_PKG_VERSION");

    scalar to_braille(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "to_braille")?;
        Ok(NeutralValue::Text(logic::to_braille(&s)))
    };
}
