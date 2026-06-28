//! Neutral core for the `talib` technical-indicator pack — written ONCE.
//!
//! TA-Lib-style indicators expressed as WINDOW functions: each indicator
//! is computed over the rows in the *current frame*. The frame is chosen
//! by the SQL author with an `OVER (...)` clause (e.g.
//! `sma(close) OVER (ORDER BY t ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)`
//! is a 3-period SMA). The period is therefore the frame width, not a
//! separate argument — the natural fit for the aggregate+frame window
//! model that both target engines expose.
//!
//! # Why "aggregate over the frame" is the portable shape
//!
//! The same neutral [`dispatch_aggregate`](datalink_extcore::ExtCore::dispatch_aggregate)
//! fold drives BOTH databases:
//!
//!   * **ducklink / DuckDB** buffers each window frame and makes ONE
//!     `call-aggregate-window` (engine-resolved frame) — the fold runs
//!     over exactly the frame's rows.
//!   * **sqlink / SQLite** drives the frame incrementally via
//!     `create_window_function` (xStep / xInverse / xValue / xFinal). The
//!     generated window shim keeps the current frame's rows in a per-
//!     context buffer (step pushes, inverse pops the oldest) and re-runs
//!     this same fold over the buffer for each `value()` — so the core
//!     never needs a bespoke inverse algorithm.
//!
//! Each indicator's `step` collects the (ordered) numeric values of the
//! frame; `finalize` computes the indicator over them. Order is the order
//! the engine feeds rows, which is the window's `ORDER BY`.
//!
//! # Scope (this pass)
//!
//! `sma`, `ema`, `rsi` are implemented and proven in BOTH DBs. `macd`
//! (EMA-fast minus EMA-slow with two distinct periods, classically a
//! 3-line output) does not map to a single frame/single-output window
//! aggregate and is documented as deferred — see README in the talib
//! component dirs.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// Coerce a neutral value to `f64`, accepting the arms an engine can
/// produce for a `DOUBLE`-typed window arg (the host coerces the column
/// to the registered `Float64`; an integer column/literal widens). TEXT
/// is parsed as a fallback. Anything else (incl. NULL) yields `None`, so
/// `step` skips it (matching the other datalink aggregate cores).
fn num(v: &NeutralValue) -> Option<f64> {
    match v {
        NeutralValue::Float64(x) => Some(*x),
        NeutralValue::Int64(x) => Some(*x as f64),
        NeutralValue::Text(s) => s.parse().ok(),
        _ => None,
    }
}

/// Running accumulator for one indicator over a frame: the frame's
/// numeric values, in feed order.
#[derive(Default)]
pub struct Frame {
    pub values: Vec<f64>,
}

/// `step`: append this row's first arg (the price/series value) if it is
/// numeric. NULLs and non-numeric values are skipped, matching SQL window
/// semantics where NULL rows do not contribute.
pub fn frame_step(st: &mut Frame, row: &[NeutralValue]) {
    if let Some(v) = row.first().and_then(num) {
        st.values.push(v);
    }
}

/// Simple moving average: the mean of the frame's values.
pub fn sma(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let sum: f64 = values.iter().sum();
    Some(sum / values.len() as f64)
}

/// Exponential moving average over the frame, smoothing
/// `alpha = 2 / (N + 1)` where `N` is the number of values in the frame
/// (the standard windowed EMA). Seeded with the first value.
pub fn ema(values: &[f64]) -> Option<f64> {
    let n = values.len();
    if n == 0 {
        return None;
    }
    let alpha = 2.0 / (n as f64 + 1.0);
    let mut e = values[0];
    for &x in &values[1..] {
        e = alpha * x + (1.0 - alpha) * e;
    }
    Some(e)
}

/// Relative Strength Index over the frame, using the simple-average form
/// of average gain / average loss across the frame's consecutive deltas.
/// `RSI = 100 - 100 / (1 + RS)`, `RS = avg_gain / avg_loss`.
///   * fewer than 2 values -> `None` (no delta to measure);
///   * no losses -> `100` (canonical RSI saturation).
pub fn rsi(values: &[f64]) -> Option<f64> {
    let n = values.len();
    if n < 2 {
        return None;
    }
    let mut gain = 0.0;
    let mut loss = 0.0;
    for w in values.windows(2) {
        let d = w[1] - w[0];
        if d > 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    let denom = (n - 1) as f64;
    let avg_gain = gain / denom;
    let avg_loss = loss / denom;
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

/// Wrap an `Option<f64>` indicator result as a neutral value (`None` ->
/// SQL NULL).
fn out(v: Option<f64>) -> NeutralValue {
    v.map(NeutralValue::Float64).unwrap_or(NeutralValue::Null)
}

datalink_extcore::declare! {
    core = Core;
    extension = "talib";
    version = env!("CARGO_PKG_VERSION");

    // sma(value) OVER (...) — simple moving average over the frame.
    aggregate sma(float64) -> float64 [deterministic] {
        state = Frame;
        init = Frame::default();
        step = frame_step;
        finalize = |st: Frame| Ok(out(sma(&st.values)));
    }

    // ema(value) OVER (...) — exponential moving average over the frame.
    aggregate ema(float64) -> float64 [deterministic] {
        state = Frame;
        init = Frame::default();
        step = frame_step;
        finalize = |st: Frame| Ok(out(ema(&st.values)));
    }

    // rsi(value) OVER (...) — relative strength index over the frame.
    aggregate rsi(float64) -> float64 [deterministic] {
        state = Frame;
        init = Frame::default();
        step = frame_step;
        finalize = |st: Frame| Ok(out(rsi(&st.values)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_basic() {
        assert_eq!(sma(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(sma(&[]), None);
    }

    #[test]
    fn ema_basic() {
        // N=3 -> alpha = 0.5. e=1; e=0.5*2+0.5*1=1.5; e=0.5*3+0.5*1.5=2.25
        assert_eq!(ema(&[1.0, 2.0, 3.0]), Some(2.25));
        assert_eq!(ema(&[5.0]), Some(5.0));
        assert_eq!(ema(&[]), None);
    }

    #[test]
    fn rsi_basic() {
        // strictly rising -> no losses -> 100.
        assert_eq!(rsi(&[1.0, 2.0, 3.0, 4.0]), Some(100.0));
        assert_eq!(rsi(&[5.0]), None);
        // 1,2,1: gains=1 (1->2), losses=1 (2->1); avg over 2 deltas each
        // 0.5; RS=1; RSI=50.
        assert_eq!(rsi(&[1.0, 2.0, 1.0]), Some(50.0));
    }
}
