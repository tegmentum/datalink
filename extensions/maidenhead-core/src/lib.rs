//! Neutral core for the `maidenhead` extension — Maidenhead grid locator
//! (ham radio) — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `to_maidenhead(lat, lon, precision) -> text` (precision = pairs)
//!   * `maidenhead_lat(grid) -> float64`  (square center latitude)
//!   * `maidenhead_lon(grid) -> float64`  (square center longitude)
//!
//! NULL / invalid -> NULL. A NULL/non-positive precision falls back to 3.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup math (DB-agnostic).
pub mod logic {
    use alloc::string::String;
    use alloc::vec::Vec;

    fn pair_base(i: usize) -> u32 {
        match i {
            0 => 18,
            1 => 10,
            _ => {
                if i % 2 == 0 {
                    24
                } else {
                    10
                }
            }
        }
    }

    pub fn encode(lat: f64, lon: f64, pairs: usize) -> Option<String> {
        if !lat.is_finite() || !lon.is_finite() {
            return None;
        }
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return None;
        }
        if pairs == 0 || pairs > 10 {
            return None;
        }
        let mut lon_u = lon + 180.0;
        let mut lat_u = lat + 90.0;
        let mut lon_cell = 360.0;
        let mut lat_cell = 180.0;
        let mut out = String::new();
        for i in 0..pairs {
            let base = pair_base(i) as f64;
            lon_cell /= base;
            lat_cell /= base;
            let mut xi = (lon_u / lon_cell).floor();
            let mut yi = (lat_u / lat_cell).floor();
            let maxv = base - 1.0;
            if xi < 0.0 {
                xi = 0.0;
            } else if xi > maxv {
                xi = maxv;
            }
            if yi < 0.0 {
                yi = 0.0;
            } else if yi > maxv {
                yi = maxv;
            }
            out.push(digit(i, xi as u32));
            out.push(digit(i, yi as u32));
            lon_u -= xi * lon_cell;
            lat_u -= yi * lat_cell;
        }
        Some(out)
    }

    fn digit(i: usize, v: u32) -> char {
        match pair_base(i) {
            18 => (b'A' + v as u8) as char,
            10 => (b'0' + v as u8) as char,
            _ => (b'a' + v as u8) as char,
        }
    }

    fn char_index(i: usize, c: char) -> Option<u32> {
        let base = pair_base(i);
        let v: u32 = match base {
            18 => {
                let u = c.to_ascii_uppercase();
                if !u.is_ascii_uppercase() {
                    return None;
                }
                (u as u8 - b'A') as u32
            }
            10 => {
                if !c.is_ascii_digit() {
                    return None;
                }
                (c as u8 - b'0') as u32
            }
            _ => {
                let l = c.to_ascii_lowercase();
                if !l.is_ascii_lowercase() {
                    return None;
                }
                (l as u8 - b'a') as u32
            }
        };
        if v >= base {
            None
        } else {
            Some(v)
        }
    }

    pub fn decode(grid: &str) -> Option<(f64, f64)> {
        let chars: Vec<char> = grid.trim().chars().collect();
        if chars.is_empty() || chars.len() % 2 != 0 {
            return None;
        }
        let pairs = chars.len() / 2;
        if pairs > 10 {
            return None;
        }
        let mut lon = 0.0_f64;
        let mut lat = 0.0_f64;
        let mut lon_cell = 360.0_f64;
        let mut lat_cell = 180.0_f64;
        for i in 0..pairs {
            let base = pair_base(i) as f64;
            lon_cell /= base;
            lat_cell /= base;
            let cx = chars[2 * i];
            let cy = chars[2 * i + 1];
            let xi = char_index(i, cx)? as f64;
            let yi = char_index(i, cy)? as f64;
            lon += xi * lon_cell;
            lat += yi * lat_cell;
        }
        lon += lon_cell / 2.0 - 180.0;
        lat += lat_cell / 2.0 - 90.0;
        Some((lat, lon))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "maidenhead";
    version = env!("CARGO_PKG_VERSION");

    scalar to_maidenhead(float64, float64, int64) -> text [propagate, deterministic] = |args| {
        let lat = args.arg_float(0, "to_maidenhead")?;
        let lon = args.arg_float(1, "to_maidenhead")?;
        let pairs = match args.arg_int(2, "to_maidenhead") {
            Ok(n) if n > 0 => n as usize,
            _ => 3,
        };
        Ok(match logic::encode(lat, lon, pairs) {
            Some(s) => NeutralValue::Text(s),
            None => NeutralValue::Null,
        })
    };

    scalar maidenhead_lat(text) -> float64 [propagate, deterministic] = |args| {
        let grid = args.arg_text(0, "maidenhead_lat")?;
        Ok(match logic::decode(&grid) {
            Some((lat, _)) => NeutralValue::Float64(lat),
            None => NeutralValue::Null,
        })
    };

    scalar maidenhead_lon(text) -> float64 [propagate, deterministic] = |args| {
        let grid = args.arg_text(0, "maidenhead_lon")?;
        Ok(match logic::decode(&grid) {
            Some((_, lon)) => NeutralValue::Float64(lon),
            None => NeutralValue::Null,
        })
    };
}
