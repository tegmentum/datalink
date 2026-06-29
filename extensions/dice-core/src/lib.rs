//! Neutral core for the `dice` extension — RPG dice notation (via `rand`) —
//! written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `dice_roll(notation) -> int64`  random roll (e.g. "2d6+3")
//!   * `dice_min(notation)  -> int64`  deterministic lower bound
//!   * `dice_max(notation)  -> int64`  deterministic upper bound
//!
//! Bad notation -> `NULL`, byte-for-byte the pre-pullup behaviour.
//! `dice_roll` is declared `nondeterministic`.
//!
//! `std` (not `no_std`): `rand`'s thread RNG is a std facility; `extern crate
//! alloc` keeps the `declare!`-generated `::alloc` paths resolvable.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Parse "[N]dM[(+|-)K]" into (count, sides, modifier) (DB-agnostic).
pub fn parse(s: &str) -> Option<(i64, i64, i64)> {
    let s: alloc::string::String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<alloc::string::String>()
        .to_ascii_lowercase();
    let (cnt, rest) = s.split_once('d')?;
    let count: i64 = if cnt.is_empty() { 1 } else { cnt.parse().ok()? };
    let (sides, modifier) = match rest.find(['+', '-']) {
        Some(i) => (rest[..i].parse().ok()?, rest[i..].parse().ok()?),
        None => (rest.parse().ok()?, 0),
    };
    if count < 1 || count > 1000 || sides < 1 {
        return None;
    }
    Some((count, sides, modifier))
}

/// Roll `count` dice of `sides`, plus `modifier` (random; std thread RNG).
pub fn roll(count: i64, sides: i64, modifier: i64) -> i64 {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let sum: i64 = (0..count).map(|_| rng.gen_range(1..=sides)).sum();
    sum + modifier
}

datalink_extcore::declare! {
    core = Core;
    extension = "dice";
    version = env!("CARGO_PKG_VERSION");

    scalar dice_roll(text) -> int64 [propagate, nondeterministic] = |args| {
        let n = args.arg_text(0, "dice_roll")?;
        Ok(match parse(&n) {
            Some((c, s, m)) => NeutralValue::Int64(roll(c, s, m)),
            None => NeutralValue::Null,
        })
    };

    scalar dice_min(text) -> int64 [propagate, deterministic] = |args| {
        let n = args.arg_text(0, "dice_min")?;
        Ok(match parse(&n) {
            Some((c, _s, m)) => NeutralValue::Int64(c + m),
            None => NeutralValue::Null,
        })
    };

    scalar dice_max(text) -> int64 [propagate, deterministic] = |args| {
        let n = args.arg_text(0, "dice_max")?;
        Ok(match parse(&n) {
            Some((c, s, m)) => NeutralValue::Int64(c * s + m),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::String;
    use datalink_extcore::ExtCore;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }
    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }

    #[test]
    fn parity_with_baseline_smoke() {
        assert_eq!(
            Core::dispatch(idx("dice_min"), &[t("2d6+3")]).unwrap(),
            NeutralValue::Int64(5)
        );
        assert_eq!(
            Core::dispatch(idx("dice_max"), &[t("2d6+3")]).unwrap(),
            NeutralValue::Int64(15)
        );
        assert_eq!(
            Core::dispatch(idx("dice_max"), &[t("d20")]).unwrap(),
            NeutralValue::Int64(20)
        );
        match Core::dispatch(idx("dice_roll"), &[t("3d6")]).unwrap() {
            NeutralValue::Int64(v) => assert!((3..=18).contains(&v), "out of range: {v}"),
            other => panic!("expected int, got {other:?}"),
        }
        assert_eq!(
            Core::dispatch(idx("dice_min"), &[t("garbage")]).unwrap(),
            NeutralValue::Null
        );
    }
}
