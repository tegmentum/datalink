//! Neutral core for the `checkdigit` extension — the Verhoeff and Damm
//! check-digit schemes (which, unlike Luhn, catch all single-digit and
//! adjacent-transposition errors) — written ONCE. The per-DB shims
//! (ducklink `duckdb:extension`, sqlink `sqlite:extension`) are generated
//! from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Functions
//!
//!   * `verhoeff_validate(text) -> boolean`
//!   * `verhoeff_append(text)   -> text`
//!   * `damm_validate(text)     -> boolean`
//!   * `damm_append(text)       -> text`
//!
//! Non-digit characters are stripped. An empty digit string validates to
//! `false` and appends to `NULL`, byte-for-byte the pre-pullup behaviour.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    /// Dihedral group D5 multiplication table.
    pub const D: [[u8; 10]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
        [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
        [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
        [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
        [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
        [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
        [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
        [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
        [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    ];
    /// Verhoeff permutation table.
    pub const P: [[u8; 10]; 8] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
        [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
        [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
        [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
        [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
        [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
        [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
    ];
    /// Verhoeff inverse table.
    pub const INV: [u8; 10] = [0, 4, 3, 2, 1, 5, 6, 7, 8, 9];
    /// Damm quasigroup operation table (totally anti-symmetric).
    pub const DAMM: [[u8; 10]; 10] = [
        [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
        [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
        [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
        [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
        [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
        [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
        [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
        [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
        [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
        [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
    ];

    /// Collect base-10 digits, ignoring any other character.
    pub fn digits(s: &str) -> Vec<u8> {
        s.chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect()
    }

    /// Verhoeff running check. `for_append` shifts the permutation row so the
    /// appended check digit lands in position 0.
    pub fn verhoeff_check(ds: &[u8], for_append: bool) -> u8 {
        let mut c = 0u8;
        for (i, &d) in ds.iter().rev().enumerate() {
            let row = if for_append { (i + 1) % 8 } else { i % 8 };
            c = D[c as usize][P[row][d as usize] as usize];
        }
        c
    }

    /// Damm interim digit (0 means valid).
    pub fn damm_interim(ds: &[u8]) -> u8 {
        let mut interim = 0u8;
        for &d in ds {
            interim = DAMM[interim as usize][d as usize];
        }
        interim
    }

    /// Render digits back to a string.
    pub fn to_str(ds: &[u8]) -> String {
        ds.iter().map(|d| (b'0' + d) as char).collect()
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "checkdigit";
    version = env!("CARGO_PKG_VERSION");

    scalar verhoeff_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "verhoeff_validate")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Boolean(false));
        }
        Ok(NeutralValue::Boolean(logic::verhoeff_check(&ds, false) == 0))
    };

    scalar verhoeff_append(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "verhoeff_append")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Null);
        }
        let cd = logic::INV[logic::verhoeff_check(&ds, true) as usize];
        Ok(NeutralValue::Text(alloc::format!("{}{}", logic::to_str(&ds), cd)))
    };

    scalar damm_validate(text) -> boolean [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "damm_validate")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Boolean(false));
        }
        Ok(NeutralValue::Boolean(logic::damm_interim(&ds) == 0))
    };

    scalar damm_append(text) -> text [propagate, deterministic] = |args| {
        let raw = args.arg_text(0, "damm_append")?;
        let ds = logic::digits(&raw);
        if ds.is_empty() {
            return Ok(NeutralValue::Null);
        }
        let cd = logic::damm_interim(&ds);
        Ok(NeutralValue::Text(alloc::format!("{}{}", logic::to_str(&ds), cd)))
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
        // Verhoeff of 236 -> check 3 => 2363 valid.
        assert_eq!(
            Core::dispatch(idx("verhoeff_append"), &[t("236")]).unwrap(),
            NeutralValue::Text(String::from("2363"))
        );
        assert_eq!(
            Core::dispatch(idx("verhoeff_validate"), &[t("2363")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("verhoeff_validate"), &[t("2364")]).unwrap(),
            NeutralValue::Boolean(false)
        );
        // Damm of 572 -> 4 => 5724 valid.
        assert_eq!(
            Core::dispatch(idx("damm_append"), &[t("572")]).unwrap(),
            NeutralValue::Text(String::from("5724"))
        );
        assert_eq!(
            Core::dispatch(idx("damm_validate"), &[t("5724")]).unwrap(),
            NeutralValue::Boolean(true)
        );
        assert_eq!(
            Core::dispatch(idx("damm_validate"), &[t("5720")]).unwrap(),
            NeutralValue::Boolean(false)
        );
    }

    #[test]
    fn empty_digits_validate_false_append_null() {
        assert_eq!(
            Core::dispatch(idx("verhoeff_validate"), &[t("not digits")]).unwrap(),
            NeutralValue::Boolean(false)
        );
        assert_eq!(
            Core::dispatch(idx("damm_append"), &[t("")]).unwrap(),
            NeutralValue::Null
        );
    }
}
