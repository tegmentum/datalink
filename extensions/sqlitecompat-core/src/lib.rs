//! Neutral core for the `sqlitecompat` cross-compat pack — SQLite
//! built-in scalar functions that DuckDB does NOT provide, written ONCE.
//! The per-DB shim is GENERATED for ducklink ONLY (see the
//! `sqlitecompat-component` crate), so a DuckDB user gets the SQLite
//! names + semantics.
//!
//! # Direction (#153 cross-compat): SQLite -> DuckDB
//!
//! These are SQLite builtins; SQLite already ships them, so there is NO
//! sqlink shim — only the ducklink one. Each name was verified absent
//! from DuckDB's builtin scalar set (`grep` over
//! `external/duckdb/{src,extension/core_functions}`): re-registering a
//! same-signature builtin is LOAD-FATAL, so only genuine gaps are here.
//! (DuckDB DOES ship `instr`/`unicode`/`printf`/`hex`/`unhex`/`typeof`/
//! `chr`/`length`/`replace`/`substr` — those are deliberately NOT here.)
//!
//! # Functions
//!
//!   * `zeroblob(n) -> blob`           — `n` zero bytes (negative -> empty).
//!   * `randomblob(n) -> blob`         — `n` pseudo-random bytes
//!                                       (`n < 1` -> 1 byte, per SQLite).
//!   * `likely(x) -> boolean`          — identity; an optimizer hint.
//!   * `unlikely(x) -> boolean`        — identity; an optimizer hint.
//!   * `likelihood(x, prob) -> boolean`— returns `x`; `prob` is a hint.
//!
//! SQLite's `likely(X)`/`unlikely(X)`/`likelihood(X,Y)` are no-op
//! optimizer hints that return their first argument unchanged; in
//! practice they wrap a boolean predicate, so they are typed
//! `boolean -> boolean` here. NULL propagates to NULL via the shim.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic implementations. Native-testable; the generated shim is a
/// thin dispatch wrapper over [`Core`].
pub mod logic {
    use alloc::vec;
    use alloc::vec::Vec;

    /// `n` zero bytes; a non-positive `n` yields an empty blob (SQLite
    /// treats `zeroblob(<=0)` as a zero-length blob).
    pub fn zeroblob(n: i64) -> Vec<u8> {
        if n <= 0 {
            Vec::new()
        } else {
            vec![0u8; n as usize]
        }
    }

    /// `n` pseudo-random bytes. SQLite returns a 1-byte blob when
    /// `n < 1`. A lightweight xorshift64 LCG (no `rand` crate), matching
    /// the math core's `rand` style — good for SQL-side sampling, not
    /// cryptography. Nondeterministic by declaration.
    pub fn randomblob(n: i64) -> Vec<u8> {
        use core::sync::atomic::{AtomicU64, Ordering};
        static STATE: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);
        let len = if n < 1 { 1usize } else { n as usize };
        let mut out = Vec::with_capacity(len);
        let mut v = STATE.load(Ordering::Relaxed);
        while out.len() < len {
            v ^= v << 13;
            v ^= v >> 7;
            v ^= v << 17;
            let bytes = v.to_le_bytes();
            for &b in bytes.iter() {
                if out.len() == len {
                    break;
                }
                out.push(b);
            }
        }
        STATE.store(v, Ordering::Relaxed);
        out
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "sqlitecompat";
    version = env!("CARGO_PKG_VERSION");

    // SQLite zeroblob(N). DuckDB has no zeroblob.
    scalar zeroblob(int64) -> blob [propagate, deterministic] = |args| {
        Ok(NeutralValue::Blob(logic::zeroblob(args.arg_int(0, "zeroblob")?)))
    };

    // SQLite randomblob(N). DuckDB has no randomblob. Nondeterministic.
    scalar randomblob(int64) -> blob [propagate, nondeterministic] = |args| {
        Ok(NeutralValue::Blob(logic::randomblob(args.arg_int(0, "randomblob")?)))
    };

    // SQLite likely(X): optimizer hint, returns X. DuckDB lacks it.
    scalar likely(boolean) -> boolean [propagate, deterministic] = |args| {
        match args.first() {
            Some(NeutralValue::Boolean(b)) => Ok(NeutralValue::Boolean(*b)),
            _ => Err(::alloc::string::String::from("likely: expected boolean arg")),
        }
    };

    // SQLite unlikely(X): optimizer hint, returns X. DuckDB lacks it.
    scalar unlikely(boolean) -> boolean [propagate, deterministic] = |args| {
        match args.first() {
            Some(NeutralValue::Boolean(b)) => Ok(NeutralValue::Boolean(*b)),
            _ => Err(::alloc::string::String::from("unlikely: expected boolean arg")),
        }
    };

    // SQLite likelihood(X, prob): returns X; `prob` is a hint (ignored).
    scalar likelihood(boolean, float64) -> boolean [propagate, deterministic] = |args| {
        match args.first() {
            Some(NeutralValue::Boolean(b)) => Ok(NeutralValue::Boolean(*b)),
            _ => Err(::alloc::string::String::from("likelihood: expected boolean arg")),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use datalink_extcore::ExtCore;

    fn idx(n: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == n).unwrap()
    }

    #[test]
    fn zeroblob_lengths() {
        assert_eq!(
            Core::dispatch(idx("zeroblob"), &[NeutralValue::Int64(4)]).unwrap(),
            NeutralValue::Blob(vec![0, 0, 0, 0])
        );
        assert_eq!(
            Core::dispatch(idx("zeroblob"), &[NeutralValue::Int64(-1)]).unwrap(),
            NeutralValue::Blob(Vec::new())
        );
    }

    #[test]
    fn randomblob_length_and_floor() {
        match Core::dispatch(idx("randomblob"), &[NeutralValue::Int64(8)]).unwrap() {
            NeutralValue::Blob(b) => assert_eq!(b.len(), 8),
            other => panic!("{other:?}"),
        }
        // n < 1 -> 1 byte (SQLite semantics).
        match Core::dispatch(idx("randomblob"), &[NeutralValue::Int64(0)]).unwrap() {
            NeutralValue::Blob(b) => assert_eq!(b.len(), 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hints_are_identity() {
        for n in ["likely", "unlikely"] {
            assert_eq!(
                Core::dispatch(idx(n), &[NeutralValue::Boolean(true)]).unwrap(),
                NeutralValue::Boolean(true)
            );
        }
        assert_eq!(
            Core::dispatch(
                idx("likelihood"),
                &[NeutralValue::Boolean(false), NeutralValue::Float64(0.9)]
            )
            .unwrap(),
            NeutralValue::Boolean(false)
        );
    }
}
