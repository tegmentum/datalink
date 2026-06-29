//! Neutral core for the `hmac` extension — keyed HMAC over `hmac` + `sha2`,
//! hex-encoded — written ONCE. The per-DB shims are generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `hmac_sha256(key text, msg text) -> text` — hex HMAC-SHA256.
//!   * `hmac_sha512(key text, msg text) -> text` — hex HMAC-SHA512.
//!
//! NULL in either argument -> NULL (propagate). Identical in both ports.

extern crate alloc;

use alloc::string::String;
use datalink_extcore::{ArgExt, NeutralValue};
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512};

pub fn hmac256(key: &str, msg: &str) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn hmac512(key: &str, msg: &str) -> String {
    let mut mac = <Hmac<Sha512>>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

datalink_extcore::declare! {
    core = Core;
    extension = "hmac";
    version = env!("CARGO_PKG_VERSION");

    scalar hmac_sha256(text, text) -> text [propagate, deterministic] = |args| {
        let key = args.arg_text(0, "hmac_sha256")?;
        let msg = args.arg_text(1, "hmac_sha256")?;
        Ok(NeutralValue::Text(hmac256(&key, &msg)))
    };

    scalar hmac_sha512(text, text) -> text [propagate, deterministic] = |args| {
        let key = args.arg_text(0, "hmac_sha512")?;
        let msg = args.arg_text(1, "hmac_sha512")?;
        Ok(NeutralValue::Text(hmac512(&key, &msg)))
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use std::vec;

    fn idx(name: &str) -> usize {
        Core::DECLS.iter().position(|d| d.name == name).unwrap()
    }
    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }

    #[test]
    fn known_vector() {
        // RFC-4231 test case 1.
        assert_eq!(
            Core::dispatch(idx("hmac_sha256"), &[t("key"), t("The quick brown fox jumps over the lazy dog")]).unwrap(),
            t("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8")
        );
    }
}
