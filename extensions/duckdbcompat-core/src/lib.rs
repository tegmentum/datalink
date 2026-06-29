//! Neutral core for the `duckdbcompat` cross-compat pack — DuckDB-native
//! scalar functions that SQLite does NOT provide, written ONCE. The
//! per-DB shim is GENERATED for sqlink ONLY (see the `duckdbcompat`
//! extension crate), so a SQLite user gets the DuckDB names + semantics.
//!
//! # Direction (#153 cross-compat): DuckDB -> SQLite
//!
//! These are DuckDB builtins; DuckDB already ships them, so there is NO
//! ducklink shim — only the sqlink one. Each name was verified absent
//! from SQLite's builtin scalar set AND from the already-pulled-up
//! sqlink packs (#152: `bit_count`, `gamma_cdf`/`gamma_pdf`, the
//! regexp/list/stdsql sweep already exist — those are NOT re-declared
//! here). What remains are genuine DuckDB-only gaps:
//!
//!   * `bar(x, min, max) -> text`        — unicode bar, width 80.
//!   * `bar(x, min, max, width) -> text` — unicode bar, given width.
//!   * `even(x) -> float64`              — round away from zero to the
//!                                         next even integer.
//!   * `gamma(x) -> float64`             — the gamma function (tgamma).
//!   * `lgamma(x) -> float64`            — natural log of |gamma(x)|.
//!   * `nextafter(x, y) -> float64`      — next double after `x` toward `y`.
//!
//! `bar` is a faithful port of DuckDB's `BarScalarFunction`
//! (`extension/core_functions/scalar/string/bar.cpp` + the
//! `UnicodeBar` block glyphs). NULL propagates to NULL via the shim.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic implementations. Native-testable; the generated shim is a
/// thin dispatch wrapper over [`Core`].
pub mod logic {
    use alloc::string::String;

    /// U+2588 FULL BLOCK.
    const FULL_BLOCK: &str = "\u{2588}";
    /// Eighth-block partials, index 0 = space .. 7 = ▉ (matches DuckDB's
    /// `UnicodeBar::PartialBlocks()`).
    const PARTIAL_BLOCKS: [&str; 8] = [
        " ",
        "\u{258F}", // ▏
        "\u{258E}", // ▎
        "\u{258D}", // ▍
        "\u{258C}", // ▌
        "\u{258B}", // ▋
        "\u{258A}", // ▊
        "\u{2589}", // ▉
    ];
    const PARTIAL_BLOCKS_COUNT: u32 = 8;

    /// Faithful port of DuckDB's `bar(x, min, max, max_width)`.
    ///
    /// `Err` mirrors DuckDB's `OutOfRangeException` cases (NaN/inf width,
    /// width < 1 or > 1000); the sqlink shim turns the error into NULL or
    /// a SQLite error per its convention.
    pub fn bar(x: f64, min: f64, max: f64, max_width: f64) -> Result<String, String> {
        if !max_width.is_finite() {
            return Err(String::from("bar: max width must not be NaN or infinity"));
        }
        if max_width < 1.0 {
            return Err(String::from("bar: max width must be >= 1"));
        }
        if max_width > 1000.0 {
            return Err(String::from("bar: max width must be <= 1000"));
        }

        let width = if x.is_nan() || min.is_nan() || max.is_nan() || x <= min {
            0.0
        } else if x >= max {
            max_width
        } else {
            max_width * (x - min) / (max - min)
        };

        if !width.is_finite() {
            return Err(String::from("bar: bar width must not be NaN or infinity"));
        }

        let mut result = String::new();
        let mut used_blocks: u32 = 0;

        // LossyNumericCast<uint32_t>: truncate toward zero, clamp to >= 0.
        let scaled = width * PARTIAL_BLOCKS_COUNT as f64;
        let width_as_int: u32 = if scaled <= 0.0 { 0 } else { scaled as u32 };

        let full_blocks_count = width_as_int / PARTIAL_BLOCKS_COUNT;
        for _ in 0..full_blocks_count {
            used_blocks += 1;
            result.push_str(FULL_BLOCK);
        }

        let remaining = (width_as_int % PARTIAL_BLOCKS_COUNT) as usize;
        if remaining != 0 {
            used_blocks += 1;
            result.push_str(PARTIAL_BLOCKS[remaining]);
        }

        let integer_max_width = max_width as u32; // truncate
        if used_blocks < integer_max_width {
            for _ in 0..(integer_max_width - used_blocks) {
                result.push(' ');
            }
        }
        Ok(result)
    }

    /// DuckDB `even(x)`: round away from zero to the next even integer.
    pub fn even(x: f64) -> f64 {
        if x == 0.0 {
            return 0.0;
        }
        let mag = libm::ceil(libm::fabs(x) / 2.0) * 2.0;
        libm::copysign(mag, x)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "duckdbcompat";
    version = env!("CARGO_PKG_VERSION");

    // DuckDB bar(x, min, max) -- width 80.
    scalar bar(float64, float64, float64) -> text [propagate, deterministic] = |args| {
        let x = args.arg_float(0, "bar")?;
        let lo = args.arg_float(1, "bar")?;
        let hi = args.arg_float(2, "bar")?;
        Ok(NeutralValue::Text(logic::bar(x, lo, hi, 80.0)?))
    };

    // DuckDB bar(x, min, max, width).
    scalar bar(float64, float64, float64, float64) -> text [propagate, deterministic] = |args| {
        let x = args.arg_float(0, "bar")?;
        let lo = args.arg_float(1, "bar")?;
        let hi = args.arg_float(2, "bar")?;
        let w = args.arg_float(3, "bar")?;
        Ok(NeutralValue::Text(logic::bar(x, lo, hi, w)?))
    };

    // DuckDB even(x): round away from zero to the next even integer.
    scalar even(float64) -> float64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Float64(logic::even(args.arg_float(0, "even")?)))
    };

    // DuckDB gamma(x): the gamma function.
    scalar gamma(float64) -> float64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Float64(libm::tgamma(args.arg_float(0, "gamma")?)))
    };

    // DuckDB lgamma(x): natural log of |gamma(x)|.
    scalar lgamma(float64) -> float64 [propagate, deterministic] = |args| {
        Ok(NeutralValue::Float64(libm::lgamma(args.arg_float(0, "lgamma")?)))
    };

    // DuckDB nextafter(x, y): next representable double after x toward y.
    scalar nextafter(float64, float64) -> float64 [propagate, deterministic] = |args| {
        let x = args.arg_float(0, "nextafter")?;
        let y = args.arg_float(1, "nextafter")?;
        Ok(NeutralValue::Float64(libm::nextafter(x, y)))
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
    fn bar_matches_duckdb() {
        // bar(5, 0, 10, 10): width = 10*5/10 = 5.0; 5*8=40 eighths ->
        // 5 full blocks, no remainder, padded to 10.
        let r = match Core::dispatch(
            idx_arity("bar", 4),
            &[
                NeutralValue::Float64(5.0),
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(10.0),
                NeutralValue::Float64(10.0),
            ],
        )
        .unwrap()
        {
            NeutralValue::Text(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(r, "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}     ");
        assert_eq!(r.chars().count(), 10);

        // x <= min -> all spaces.
        let r0 = match Core::dispatch(
            idx_arity("bar", 4),
            &[
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(10.0),
                NeutralValue::Float64(4.0),
            ],
        )
        .unwrap()
        {
            NeutralValue::Text(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(r0, "    ");

        // x >= max -> all full blocks.
        let rf = match Core::dispatch(
            idx_arity("bar", 3),
            &[
                NeutralValue::Float64(20.0),
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(10.0),
            ],
        )
        .unwrap()
        {
            NeutralValue::Text(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(rf.chars().count(), 80);
        assert!(rf.chars().all(|c| c == '\u{2588}'));
    }

    #[test]
    fn bar_partial_block() {
        // bar(1, 0, 8, 8): width = 1.0; 1*8 = 8 eighths -> 1 full block.
        // bar(0.5, 0, 8, 8): width = 0.5; 0.5*8 = 4 eighths -> partial[4].
        let r = match Core::dispatch(
            idx_arity("bar", 4),
            &[
                NeutralValue::Float64(0.5),
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(8.0),
                NeutralValue::Float64(8.0),
            ],
        )
        .unwrap()
        {
            NeutralValue::Text(s) => s,
            other => panic!("{other:?}"),
        };
        // 4 eighths -> partial[4] = ▌, then padded to width 8.
        assert!(r.starts_with("\u{258C}"));
        assert_eq!(r.chars().count(), 8);
    }

    #[test]
    fn bar_width_bounds_error() {
        assert!(Core::dispatch(
            idx_arity("bar", 4),
            &[
                NeutralValue::Float64(1.0),
                NeutralValue::Float64(0.0),
                NeutralValue::Float64(10.0),
                NeutralValue::Float64(0.0),
            ],
        )
        .is_err());
    }

    #[test]
    fn even_rounds_away_to_even() {
        let cases = [
            (3.0, 4.0),
            (2.0, 2.0),
            (2.5, 4.0),
            (-1.1, -2.0),
            (0.0, 0.0),
            (-3.0, -4.0),
        ];
        for (x, want) in cases {
            match Core::dispatch(idx_arity("even", 1), &[NeutralValue::Float64(x)]).unwrap() {
                NeutralValue::Float64(v) => assert!(approx(v, want), "even({x}) = {v}, want {want}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn gamma_and_lgamma() {
        // gamma(5) = 4! = 24.
        match Core::dispatch(idx_arity("gamma", 1), &[NeutralValue::Float64(5.0)]).unwrap() {
            NeutralValue::Float64(v) => assert!(approx(v, 24.0)),
            other => panic!("{other:?}"),
        }
        // lgamma(1) = ln(0!) = 0.
        match Core::dispatch(idx_arity("lgamma", 1), &[NeutralValue::Float64(1.0)]).unwrap() {
            NeutralValue::Float64(v) => assert!(approx(v, 0.0)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nextafter_steps() {
        match Core::dispatch(
            idx_arity("nextafter", 2),
            &[NeutralValue::Float64(1.0), NeutralValue::Float64(2.0)],
        )
        .unwrap()
        {
            NeutralValue::Float64(v) => assert!(v > 1.0 && v - 1.0 < 1e-15),
            other => panic!("{other:?}"),
        }
    }
}
