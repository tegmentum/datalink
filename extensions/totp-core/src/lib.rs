//! Neutral core for the `totp` extension — RFC 6238 TOTP codes
//! (HMAC-SHA1 over a base32 secret) — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `totp(secret_base32, unix_time, period, digits) -> text`
//!
//! Deterministic given an explicit time. NULL / bad secret -> NULL.
//! `unix_time` / `period` / `digits` default to 0 / 30 / 6 when not an
//! integer (matching the pre-pullup behaviour). The surface is identical
//! in both ports (zero drift).

extern crate alloc;

use alloc::string::String;
use datalink_extcore::NeutralValue;

/// Compute the TOTP code. `None` on a bad period/digits/secret.
pub fn compute(secret: &str, time: i64, period: i64, digits: u32) -> Option<String> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    if period <= 0 || !(1..=9).contains(&digits) {
        return None;
    }
    let key = base32::decode(
        base32::Alphabet::Rfc4648 { padding: false },
        &secret.to_ascii_uppercase(),
    )?;
    if key.is_empty() {
        return None;
    }
    let counter = (time / period) as u64;
    let mut mac = <Hmac<Sha1>>::new_from_slice(&key).ok()?;
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let bin = ((hash[offset] as u32 & 0x7f) << 24)
        | ((hash[offset + 1] as u32) << 16)
        | ((hash[offset + 2] as u32) << 8)
        | (hash[offset + 3] as u32);
    let code = bin % 10u32.pow(digits);
    Some(alloc::format!("{:0width$}", code, width = digits as usize))
}

fn int_or(args: &[NeutralValue], i: usize, default: i64) -> i64 {
    match args.get(i) {
        Some(NeutralValue::Int64(n)) => *n,
        _ => default,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "totp";
    version = env!("CARGO_PKG_VERSION");

    // [called]: only a TEXT secret is accepted (NULL/other -> NULL); the
    // numeric args fall back to their defaults when not integers.
    scalar totp(text, int64, int64, int64) -> text [called, deterministic] = |args| {
        let secret = match args.first() {
            Some(NeutralValue::Text(s)) => s.clone(),
            _ => return Ok(NeutralValue::Null),
        };
        let time = int_or(args, 1, 0);
        let period = int_or(args, 2, 30);
        let digits = int_or(args, 3, 6).clamp(1, 9) as u32;
        Ok(match compute(&secret, time, period, digits) {
            Some(c) => NeutralValue::Text(c),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn i(n: i64) -> NeutralValue { NeutralValue::Int64(n) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        let s = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(Core::dispatch(idx("totp"), &[t(s), i(59), i(30), i(8)]).unwrap(), t("94287082"));
        assert_eq!(Core::dispatch(idx("totp"), &[t(s), i(1111111109), i(30), i(8)]).unwrap(), t("07081804"));
        assert_eq!(Core::dispatch(idx("totp"), &[t("!!notbase32!!"), i(59), i(30), i(6)]).unwrap(), NeutralValue::Null);
    }
}
