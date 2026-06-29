//! Neutral core for the `uuidx` extension — UUID extras complementing
//! DuckDB's built-in v4 `uuid()` — written ONCE. The per-DB shims (ducklink
//! `duckdb:extension`, sqlink `sqlite:extension`, sqlink embed) are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `uuid_v7() -> text`            generate a time-ordered UUIDv7
//!     (NONDETERMINISTIC; clock + wasi random — declared nondeterministic so
//!     the optimizer never constant-folds it).
//!   * `uuid_version(text) -> int64`  the version field (1..8) of a UUID.
//!   * `uuid_timestamp(text) -> int64` embedded unix-ms timestamp (v7/v1),
//!     else NULL.
//!
//! Parse failures -> NULL.

extern crate alloc;

use alloc::string::ToString;
use datalink_extcore::{ArgExt, NeutralValue};
use uuid::Uuid;

datalink_extcore::declare! {
    core = Core;
    extension = "uuidx";
    version = env!("CARGO_PKG_VERSION");

    scalar uuid_v7() -> text [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Text(Uuid::now_v7().to_string()))
    };

    scalar uuid_version(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "uuid_version")?;
        Ok(match Uuid::parse_str(&s) {
            Ok(u) => NeutralValue::Int64(u.get_version_num() as i64),
            Err(_) => NeutralValue::Null,
        })
    };

    scalar uuid_timestamp(text) -> int64 [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "uuid_timestamp")?;
        Ok(match Uuid::parse_str(&s).ok().and_then(|u| u.get_timestamp()) {
            Some(ts) => {
                let (secs, nanos) = ts.to_unix();
                NeutralValue::Int64(secs as i64 * 1000 + (nanos / 1_000_000) as i64)
            }
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn v7_shape_and_nondeterminism() {
        match Core::dispatch(idx("uuid_v7"), &[]).unwrap() {
            NeutralValue::Text(s) => assert_eq!(s.len(), 36),
            o => panic!("{o:?}"),
        }
        assert!(!Core::DECLS[idx("uuid_v7")].deterministic);
        assert!(Core::DECLS[idx("uuid_version")].deterministic);
    }

    #[test]
    fn version_and_timestamp() {
        assert_eq!(
            Core::dispatch(idx("uuid_version"), &[t("00000000-0000-4000-8000-000000000000")]).unwrap(),
            NeutralValue::Int64(4)
        );
        assert_eq!(
            Core::dispatch(idx("uuid_version"), &[t("not-a-uuid")]).unwrap(),
            NeutralValue::Null
        );
        assert_eq!(
            Core::dispatch(idx("uuid_timestamp"), &[t("017f22e2-79b0-7cc3-98c4-dc0c0c07398f")]).unwrap(),
            NeutralValue::Int64(1645557742000)
        );
        assert_eq!(
            Core::dispatch(idx("uuid_timestamp"), &[t("00000000-0000-4000-8000-000000000000")]).unwrap(),
            NeutralValue::Null
        );
    }
}
