//! Neutral core for the `ids` extension — identifier generators — written
//! ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ulid() -> text`              (NONDETERMINISTIC; wasi random + clock)
//!   * `nanoid() -> text`            (NONDETERMINISTIC; wasi random)
//!   * `ulid_timestamp(ulid) -> int64`  (epoch ms; NULL if not a valid ULID)
//!
//! `ulid` / `nanoid` are declared `nondeterministic`, so the generated
//! shim omits `Funcflags::DETERMINISTIC` and the DuckDB optimizer treats
//! them as volatile (never constant-folds them). The surface is identical
//! in both ports (zero drift).

extern crate alloc;

use alloc::string::ToString;
use core::str::FromStr;
use datalink_extcore::NeutralValue;
use ulid::Ulid;

datalink_extcore::declare! {
    core = Core;
    extension = "ids";
    version = env!("CARGO_PKG_VERSION");

    scalar ulid() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(Ulid::new().to_string()))
    };
    scalar nanoid() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(nanoid::nanoid!()))
    };
    scalar ulid_timestamp(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "ulid_timestamp")?;
        Ok(match Ulid::from_str(&s) {
            Ok(u) => NeutralValue::Int64(u.timestamp_ms() as i64),
            Err(_) => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn shape_and_determinism() {
        // ulid is 26 chars, nanoid is 21 chars.
        match Core::dispatch(idx("ulid"), &[]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.len(), 26), o => panic!("{o:?}"),
        }
        match Core::dispatch(idx("nanoid"), &[]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.len(), 21), o => panic!("{o:?}"),
        }
        assert_eq!(
            Core::dispatch(idx("ulid_timestamp"), &[NeutralValue::Text("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())]).unwrap(),
            NeutralValue::Int64(1469922850259)
        );
        assert_eq!(Core::dispatch(idx("ulid_timestamp"), &[NeutralValue::Text("not-a-ulid".into())]).unwrap(), NeutralValue::Null);
        // The generators are declared nondeterministic.
        assert!(!Core::DECLS[idx("ulid")].deterministic);
        assert!(!Core::DECLS[idx("nanoid")].deterministic);
        assert!(Core::DECLS[idx("ulid_timestamp")].deterministic);
    }
}
