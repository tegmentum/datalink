//! Neutral core for the `unitconv` extension — hand-rolled physical unit
//! conversion (length, mass, temperature) — written ONCE. The per-DB
//! shim is generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `unit_convert(value float64, from text, to text) -> float64`
//!
//! Case-insensitive unit names. Unknown unit or cross-category
//! conversion -> NULL. The surface is identical in both ports (zero
//! drift).

#![no_std]

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Linear units: factor to the category's base (metre for length, kilogram for
/// mass). Temperature is handled separately because it is affine.
fn length_factor(u: &str) -> Option<f64> {
    Some(match u {
        "m" | "metre" | "meter" => 1.0, "km" | "kilometre" | "kilometer" => 1000.0,
        "cm" | "centimetre" | "centimeter" => 0.01, "mm" | "millimetre" | "millimeter" => 0.001,
        "um" | "micron" => 1e-6, "mi" | "mile" => 1609.344, "yd" | "yard" => 0.9144,
        "ft" | "foot" | "feet" => 0.3048, "in" | "inch" => 0.0254, "nmi" => 1852.0,
        _ => return None,
    })
}
fn mass_factor(u: &str) -> Option<f64> {
    Some(match u {
        "kg" | "kilogram" => 1.0, "g" | "gram" => 0.001, "mg" | "milligram" => 1e-6,
        "t" | "tonne" => 1000.0, "lb" | "pound" => 0.453_592_37, "oz" | "ounce" => 0.028_349_523_125,
        "st" | "stone" => 6.350_293_18, _ => return None,
    })
}
fn temp_to_kelvin(v: f64, u: &str) -> Option<f64> {
    Some(match u { "c" | "celsius" => v + 273.15, "f" | "fahrenheit" => (v - 32.0) * 5.0 / 9.0 + 273.15, "k" | "kelvin" => v, _ => return None })
}
fn kelvin_to(k: f64, u: &str) -> Option<f64> {
    Some(match u { "c" | "celsius" => k - 273.15, "f" | "fahrenheit" => (k - 273.15) * 9.0 / 5.0 + 32.0, "k" | "kelvin" => k, _ => return None })
}

pub fn convert(value: f64, from: &str, to: &str) -> Option<f64> {
    let (from, to) = (from.trim().to_ascii_lowercase(), to.trim().to_ascii_lowercase());
    if let (Some(a), Some(b)) = (length_factor(&from), length_factor(&to)) { return Some(value * a / b); }
    if let (Some(a), Some(b)) = (mass_factor(&from), mass_factor(&to)) { return Some(value * a / b); }
    if let Some(k) = temp_to_kelvin(value, &from) { return kelvin_to(k, &to); }
    None
}

datalink_extcore::declare! {
    core = Core;
    extension = "unitconv";
    version = env!("CARGO_PKG_VERSION");

    scalar unit_convert(float64, text, text) -> float64 [propagate, deterministic] = |args| {
        let value = args.arg_float(0, "unit_convert")?;
        let from = args.arg_text(1, "unit_convert")?;
        let to = args.arg_text(2, "unit_convert")?;
        Ok(match convert(value, &from, &to) {
            Some(r) => NeutralValue::Float64(r),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        let r = Core::dispatch(idx("unit_convert"), &[NeutralValue::Int64(1), t("mi"), t("km")]).unwrap();
        match r { NeutralValue::Float64(v) => assert!((v - 1.609344).abs() < 1e-9), o => panic!("{o:?}") }
        assert_eq!(Core::dispatch(idx("unit_convert"), &[NeutralValue::Int64(1), t("kg"), t("m")]).unwrap(), NeutralValue::Null);
        assert_eq!(Core::dispatch(idx("unit_convert"), &[NeutralValue::Int64(1), t("foo"), t("bar")]).unwrap(), NeutralValue::Null);
    }
}
