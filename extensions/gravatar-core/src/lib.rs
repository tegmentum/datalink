//! Neutral core for the `gravatar` extension — Gravatar helpers over the
//! md5 of a normalized email — written ONCE. The per-DB shims (ducklink
//! `duckdb:extension`, sqlink `sqlite:extension`, embed) are generated from
//! the [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `gravatar_hash(email text) -> text` — md5 hex of the trimmed,
//!     lowercased email (the Gravatar normalization).
//!   * `gravatar_url(email text) -> text` — `https://www.gravatar.com/avatar/<hash>`.
//!
//! NULL email -> NULL (propagate). The surface is identical in both ports.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use datalink_extcore::NeutralValue;

/// md5 hex of the normalized (trimmed + ASCII-lowercased) email.
pub fn hash(email: &str) -> String {
    format!("{:x}", md5::compute(email.trim().to_ascii_lowercase().as_bytes()))
}

/// The Gravatar avatar URL for an email.
pub fn url(email: &str) -> String {
    format!("https://www.gravatar.com/avatar/{}", hash(email))
}

datalink_extcore::declare! {
    core = Core;
    extension = "gravatar";
    version = env!("CARGO_PKG_VERSION");

    scalar gravatar_hash(text) -> text [propagate, deterministic] = |args| {
        let email = args.arg_text(0, "gravatar_hash")?;
        Ok(NeutralValue::Text(hash(&email)))
    };

    scalar gravatar_url(text) -> text [propagate, deterministic] = |args| {
        let email = args.arg_text(0, "gravatar_url")?;
        Ok(NeutralValue::Text(url(&email)))
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
    fn hash_normalizes() {
        // Gravatar's canonical example.
        assert_eq!(hash(" MyEmailAddress@example.com "), "0bc83cb571cd1c50ba6f3e8a78ef1346");
    }

    #[test]
    fn dispatch_matches() {
        assert_eq!(
            Core::dispatch(idx("gravatar_hash"), &[NeutralValue::Text(String::from("a@b.com"))]).unwrap(),
            NeutralValue::Text(hash("a@b.com"))
        );
        assert_eq!(
            Core::dispatch(idx("gravatar_url"), &[NeutralValue::Text(String::from("a@b.com"))]).unwrap(),
            NeutralValue::Text(url("a@b.com"))
        );
    }
}
