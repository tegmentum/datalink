//! Neutral core for the `stochastic` extension — probability-distribution
//! PDF/PMF, CDF and inverse-CDF (quantile) scalars (via `statrs`) — written ONCE.
//!
//!   normal_cdf/normal_pdf/normal_quantile(x, mean, sd),
//!   binomial_pmf(k, n, p), poisson_pmf(k, lambda),
//!   exponential_cdf(x, rate), beta_cdf(x, alpha, beta).
//! NULL input (handled by the shim) or invalid params -> NULL; never panics.

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use statrs::distribution::{Beta, Binomial, Continuous, ContinuousCDF, Discrete, Exp, Normal, Poisson};

    pub fn normal_cdf(x: f64, mean: f64, sd: f64) -> Option<f64> {
        Normal::new(mean, sd).ok().map(|d| d.cdf(x))
    }
    pub fn normal_pdf(x: f64, mean: f64, sd: f64) -> Option<f64> {
        Normal::new(mean, sd).ok().map(|d| d.pdf(x))
    }
    /// quantile is the inverse CDF; `p` must be in [0, 1].
    pub fn normal_quantile(p: f64, mean: f64, sd: f64) -> Option<f64> {
        if (0.0..=1.0).contains(&p) {
            Normal::new(mean, sd).ok().map(|d| d.inverse_cdf(p))
        } else {
            None
        }
    }
    pub fn beta_cdf(x: f64, alpha: f64, beta: f64) -> Option<f64> {
        Beta::new(alpha, beta).ok().map(|d| d.cdf(x))
    }
    pub fn binomial_pmf(k: i64, n: i64, p: f64) -> Option<f64> {
        if k >= 0 && n >= 0 {
            Binomial::new(p, n as u64).ok().map(|d| d.pmf(k as u64))
        } else {
            None
        }
    }
    pub fn poisson_pmf(k: i64, lambda: f64) -> Option<f64> {
        if k >= 0 {
            Poisson::new(lambda).ok().map(|d| d.pmf(k as u64))
        } else {
            None
        }
    }
    pub fn exponential_cdf(x: f64, rate: f64) -> Option<f64> {
        Exp::new(rate).ok().map(|d| d.cdf(x))
    }
}

/// A finite `f64` -> Float64; anything else (invalid params, non-finite) -> NULL.
fn fin(v: Option<f64>) -> NeutralValue {
    match v {
        Some(x) if x.is_finite() => NeutralValue::Float64(x),
        _ => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "stochastic";
    version = env!("CARGO_PKG_VERSION");

    scalar normal_cdf(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::normal_cdf(
            args.arg_float(0, "normal_cdf")?,
            args.arg_float(1, "normal_cdf")?,
            args.arg_float(2, "normal_cdf")?,
        )))
    };

    scalar normal_pdf(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::normal_pdf(
            args.arg_float(0, "normal_pdf")?,
            args.arg_float(1, "normal_pdf")?,
            args.arg_float(2, "normal_pdf")?,
        )))
    };

    scalar normal_quantile(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::normal_quantile(
            args.arg_float(0, "normal_quantile")?,
            args.arg_float(1, "normal_quantile")?,
            args.arg_float(2, "normal_quantile")?,
        )))
    };

    scalar beta_cdf(float64, float64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::beta_cdf(
            args.arg_float(0, "beta_cdf")?,
            args.arg_float(1, "beta_cdf")?,
            args.arg_float(2, "beta_cdf")?,
        )))
    };

    scalar binomial_pmf(int64, int64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::binomial_pmf(
            args.arg_int(0, "binomial_pmf")?,
            args.arg_int(1, "binomial_pmf")?,
            args.arg_float(2, "binomial_pmf")?,
        )))
    };

    scalar poisson_pmf(int64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::poisson_pmf(
            args.arg_int(0, "poisson_pmf")?,
            args.arg_float(1, "poisson_pmf")?,
        )))
    };

    scalar exponential_cdf(float64, float64) -> float64 [propagate, deterministic] = |args| {
        Ok(fin(logic::exponential_cdf(
            args.arg_float(0, "exponential_cdf")?,
            args.arg_float(1, "exponential_cdf")?,
        )))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn approx(got: NeutralValue, want: f64) {
        match got {
            NeutralValue::Float64(v) => assert!((v - want).abs() < 1e-4, "got {v}, want {want}"),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn distributions() {
        approx(
            Core::dispatch(idx("normal_cdf"), &[NeutralValue::Float64(0.0), NeutralValue::Float64(0.0), NeutralValue::Float64(1.0)]).unwrap(),
            0.5,
        );
        approx(
            Core::dispatch(idx("binomial_pmf"), &[NeutralValue::Int64(2), NeutralValue::Int64(5), NeutralValue::Float64(0.5)]).unwrap(),
            0.3125,
        );
        // invalid sd -> NULL
        assert_eq!(
            Core::dispatch(idx("normal_cdf"), &[NeutralValue::Float64(0.0), NeutralValue::Float64(0.0), NeutralValue::Float64(0.0)]).unwrap(),
            NeutralValue::Null
        );
    }
}
