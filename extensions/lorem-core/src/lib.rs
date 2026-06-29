//! Neutral core for the `lorem` extension — lorem ipsum placeholder text
//! via the `lipsum` crate — written ONCE. The per-DB shims are generated
//! from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `lorem_words(n) -> text` — `n` words (n<1 or non-int -> 25; capped
//!     at 10000).
//!   * `lorem_title() -> text` — a title-cased lorem phrase.
//!
//! Both NONDETERMINISTIC. `lorem_words` declares CALLED null-handling so a
//! NULL `n` falls back to the default 25 (matching the pre-pullup impl)
//! rather than propagating NULL.

extern crate alloc;

use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "lorem";
    version = env!("CARGO_PKG_VERSION");

    scalar lorem_words(int64) -> text [called, nondeterministic] = |args| {
        let n = match args.first() {
            Some(NeutralValue::Int64(n)) if *n >= 1 => (*n).min(10000) as usize,
            _ => 25,
        };
        Ok(NeutralValue::Text(lipsum::lipsum(n)))
    };

    scalar lorem_title() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(lipsum::lipsum_title()))
    };
}
