//! Neutral core for the `email` extension — email validation/parsing via
//! the `email_address` crate — written ONCE. The per-DB shim is
//! generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `email_validate(text) -> boolean`  (NULL/invalid -> false)
//!   * `email_domain(text) -> text`       (NULL/invalid -> NULL)
//!   * `email_local(text) -> text`        (NULL/invalid -> NULL)
//!
//! The surface is identical in both ports (zero drift).

extern crate alloc;

use core::str::FromStr;
use datalink_extcore::NeutralValue;
use email_address::EmailAddress;

datalink_extcore::declare! {
    core = Core;
    extension = "email";
    version = env!("CARGO_PKG_VERSION");

    // [called]: a NULL coerces to "" (invalid) -> false, matching the
    // pre-pullup `email_validate(NULL) -> false`.
    scalar email_validate(text) -> boolean [called, deterministic] = |args| {
        let s = args.arg_text(0, "email_validate")?;
        Ok(NeutralValue::Boolean(EmailAddress::is_valid(&s)))
    };
    scalar email_domain(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "email_domain")?;
        Ok(match EmailAddress::from_str(&s) {
            Ok(a) => NeutralValue::Text(alloc::string::String::from(a.domain())),
            Err(_) => NeutralValue::Null,
        })
    };
    scalar email_local(text) -> text [propagate, deterministic] = |args| {
        let s = args.arg_text(0, "email_local")?;
        Ok(match EmailAddress::from_str(&s) {
            Ok(a) => NeutralValue::Text(alloc::string::String::from(a.local_part())),
            Err(_) => NeutralValue::Null,
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
        assert_eq!(Core::dispatch(idx("email_validate"), &[t("a.b@example.com")]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("email_validate"), &[t("not-an-email")]).unwrap(), NeutralValue::Boolean(false));
        assert_eq!(Core::dispatch(idx("email_domain"), &[t("user@sub.example.org")]).unwrap(), t("sub.example.org"));
        assert_eq!(Core::dispatch(idx("email_local"), &[t("user.name@example.com")]).unwrap(), t("user.name"));
        assert_eq!(Core::dispatch(idx("email_domain"), &[t("garbage")]).unwrap(), NeutralValue::Null);
    }
}
