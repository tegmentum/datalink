//! Neutral core for the `ipaddr` extension — IP validation/classification
//! via `std::net` — written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//!   * `ip_valid(text) -> boolean`       (NULL/invalid -> false)
//!   * `ip_version(text) -> int64`       (4 / 6; NULL/invalid -> NULL)
//!   * `ip_is_private(text) -> boolean`  (RFC1918 v4 / RFC4193 v6;
//!                                        NULL/invalid -> false)
//!
//! The surface is identical in both ports (zero drift).

extern crate alloc;

use core::str::FromStr;
use datalink_extcore::NeutralValue;
use std::net::{IpAddr, Ipv6Addr};

/// RFC 4193 unique-local addresses (fc00::/7); std has no stable predicate.
fn is_unique_local(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xfe00) == 0xfc00
}

datalink_extcore::declare! {
    core = Core;
    extension = "ipaddr";
    version = env!("CARGO_PKG_VERSION");

    // [called] throughout: a NULL coerces to "" (which fails to parse),
    // reproducing the pre-pullup NULL semantics (valid/private -> false,
    // version -> NULL) without a host NULL convention.
    scalar ip_valid(text) -> boolean [called, deterministic] = |args| {
        let s = args.arg_text(0, "ip_valid")?;
        Ok(NeutralValue::Boolean(IpAddr::from_str(s.trim()).is_ok()))
    };
    scalar ip_version(text) -> int64 [called, deterministic] = |args| {
        let s = args.arg_text(0, "ip_version")?;
        Ok(match IpAddr::from_str(s.trim()) {
            Ok(IpAddr::V4(_)) => NeutralValue::Int64(4),
            Ok(IpAddr::V6(_)) => NeutralValue::Int64(6),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar ip_is_private(text) -> boolean [called, deterministic] = |args| {
        let s = args.arg_text(0, "ip_is_private")?;
        Ok(match IpAddr::from_str(s.trim()) {
            Ok(IpAddr::V4(a)) => NeutralValue::Boolean(a.is_private()),
            Ok(IpAddr::V6(a)) => NeutralValue::Boolean(is_unique_local(&a)),
            Err(_) => NeutralValue::Boolean(false),
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(alloc::string::String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("ip_valid"), &[t("192.168.1.1")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("ip_valid"), &[t("2001:db8::1")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("ip_valid"), &[t("999.1.1.1")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("ip_version"), &[t("::1")]).unwrap(), NeutralValue::Int64(6));
        assert_eq!(Core::dispatch(idx("ip_is_private"), &[t("10.0.0.5")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("ip_is_private"), &[t("8.8.8.8")]).unwrap(), NeutralValue::Boolean(false));
    }
}
