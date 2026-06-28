//! Neutral core for the `math` extension — the cross-dialect scalar math
//! functions, written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the functions a host does NOT already provide
//!
//! `math` originated in sqlink because SQLite ships almost no math
//! functions. DuckDB is the opposite: its `core_functions` math library
//! already provides `ceil`/`floor`/`trunc`/`round`/`abs`/`sign`/`mod`/
//! `sqrt`/`cbrt`/`pow`/`exp`/`log{,2,10}`/the trig + hyperbolic family/
//! `degrees`/`radians`/`pi`/`cot`/`factorial`/`gcd`/`lcm`/`bit_count`/
//! `isfinite`/`width_bucket`/`bin` as BUILTINS. Re-registering any of
//! those from a component would collide with the builtin (DuckDB rejects
//! a same-signature overload), so they are deliberately NOT declared
//! here — they are the DB's own builtins on both ports.
//!
//! What remains are the genuinely-portable additions that are NOT DuckDB
//! builtins:
//!
//!   * `exp2(x) -> float64`            — 2^x
//!   * `e() -> float64`                — Euler's number
//!   * `rand() -> float64`             — uniform [0, 1) (nondeterministic)
//!   * `div(x, y) -> int64`            — integer (truncating) division
//!   * `truncate(x) -> float64`        — alias of trunc
//!   * `truncate(x, n) -> float64`     — truncate to `n` decimal places
//!
//! This is the FROZEN-NeutralType set only (Float64/Int64); NULL / a
//! missing arg propagates to NULL via the generated shim. The logic is
//! byte-identical to sqlink's `math` extension (the same libm calls and
//! the same gcd/div/truncate algorithms), so a future `sqlite_shim!`
//! over this core reproduces sqlink's behaviour for these names.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic math implementations. Native-testable; the generated shim
/// is a thin dispatch wrapper over [`Core`].
pub mod logic {
    /// Uniform pseudo-random in `[0, 1)`. A lightweight xorshift64 LCG —
    /// no `rand` crate, matching sqlink's custom LCG (good enough for
    /// SQL-side sampling, not crypto). Nondeterministic by declaration.
    pub fn rand() -> f64 {
        use core::sync::atomic::{AtomicU64, Ordering};
        static STATE: AtomicU64 = AtomicU64::new(0xCAFE_BABE_C0FF_EEEE);
        let mut v = STATE.load(Ordering::Relaxed);
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        STATE.store(v, Ordering::Relaxed);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Truncate `x` to `n` decimal places (`n` may be negative).
    pub fn truncate_n(x: f64, n: f64) -> f64 {
        let scale = libm::pow(10.0, n);
        libm::trunc(x * scale) / scale
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "math";
    version = env!("CARGO_PKG_VERSION");

    // 2^x. DuckDB has `exp` and `pow` but no `exp2`.
    scalar exp2(float64) -> float64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Float64(libm::exp2(args.arg_float(0, "exp2")?)))
    };

    // Euler's number. DuckDB exposes `pi()` but not `e()`.
    scalar e() -> float64 [propagate, deterministic] = |_args| {
        Ok(NeutralValue::Float64(core::f64::consts::E))
    };

    // Uniform [0, 1). DuckDB's builtin is `random()`; `rand` is the
    // MySQL/portable spelling and is not a DuckDB builtin.
    scalar rand() -> float64 [propagate, nondeterministic] = |_args| {
        Ok(NeutralValue::Float64(logic::rand()))
    };

    // Integer (truncating) division. DuckDB has the `//` operator but no
    // `div(x, y)` function. Division by zero is an error (NULL on the
    // SQLite side via the shim's error path).
    scalar div(int64, int64) -> int64 [propagate, deterministic] = |args| {
        let x = args.arg_int(0, "div")?;
        let y = args.arg_int(1, "div")?;
        if y == 0 {
            return Err(::alloc::string::String::from("div: division by zero"));
        }
        Ok(NeutralValue::Int64(x / y))
    };

    // truncate(x): alias of trunc (DuckDB lacks the `truncate` spelling).
    scalar truncate(float64) -> float64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Float64(libm::trunc(args.arg_float(0, "truncate")?)))
    };

    // truncate(x, n): truncate to n decimal places. DuckDB has
    // `round(x, n)` but no truncating equivalent.
    scalar truncate(float64, float64) -> float64 [propagate, deterministic] = |args| {
        let x = args.arg_float(0, "truncate")?;
        let n = args.arg_float(1, "truncate")?;
        Ok(NeutralValue::Float64(logic::truncate_n(x, n)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx_arity(n: &str, arity: usize) -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == n && d.args.len() == arity)
            .unwrap()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn exp2_and_e() {
        match Core::dispatch(idx_arity("exp2", 1), &[NeutralValue::Float64(10.0)]).unwrap() {
            NeutralValue::Float64(v) => assert!(approx(v, 1024.0)),
            other => panic!("{other:?}"),
        }
        match Core::dispatch(idx_arity("e", 0), &[]).unwrap() {
            NeutralValue::Float64(v) => assert!(approx(v, core::f64::consts::E)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn div_and_truncate() {
        assert_eq!(
            Core::dispatch(idx_arity("div", 2), &[NeutralValue::Int64(17), NeutralValue::Int64(5)])
                .unwrap(),
            NeutralValue::Int64(3)
        );
        assert!(Core::dispatch(idx_arity("div", 2), &[NeutralValue::Int64(1), NeutralValue::Int64(0)]).is_err());
        match Core::dispatch(idx_arity("truncate", 1), &[NeutralValue::Float64(3.99)]).unwrap() {
            NeutralValue::Float64(v) => assert!(approx(v, 3.0)),
            other => panic!("{other:?}"),
        }
        match Core::dispatch(
            idx_arity("truncate", 2),
            &[NeutralValue::Float64(3.14159), NeutralValue::Float64(2.0)],
        )
        .unwrap()
        {
            NeutralValue::Float64(v) => assert!(approx(v, 3.14)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rand_in_range() {
        for _ in 0..100 {
            match Core::dispatch(idx_arity("rand", 0), &[]).unwrap() {
                NeutralValue::Float64(v) => assert!((0.0..1.0).contains(&v)),
                other => panic!("{other:?}"),
            }
        }
    }
}
