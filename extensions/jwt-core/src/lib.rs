//! Neutral core for the `jwt` extension — JWT segment decoding
//! (base64url, NO signature verification) — written ONCE. The per-DB
//! shim is generated from the [`declare!`](datalink_extcore::declare)
//! table.
//!
//!   * `jwt_header(token) -> text`   (JSON header; segment 0)
//!   * `jwt_payload(token) -> text`  (JSON payload; segment 1)
//!
//! Decode only; does NOT verify the signature. Malformed / NULL -> NULL.
//! The surface is identical in both ports (zero drift).

extern crate alloc;

use alloc::string::String;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use datalink_extcore::NeutralValue;

/// Decode the `idx`-th dot-separated JWT segment as UTF-8 text.
pub fn decode_segment(token: &str, idx: usize) -> Option<String> {
    let seg = token.split('.').nth(idx)?;
    let bytes = URL_SAFE_NO_PAD.decode(seg).ok()?;
    String::from_utf8(bytes).ok()
}

fn seg(token: &str, idx: usize) -> NeutralValue {
    match decode_segment(token, idx) {
        Some(s) => NeutralValue::Text(s),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "jwt";
    version = env!("CARGO_PKG_VERSION");

    scalar jwt_header(text) -> text [propagate, deterministic] = |args| {
        Ok(seg(&args.arg_text(0, "jwt_header")?, 0))
    };
    scalar jwt_payload(text) -> text [propagate, deterministic] = |args| {
        Ok(seg(&args.arg_text(0, "jwt_payload")?, 1))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    const TOK: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    fn t(s: &str) -> NeutralValue { NeutralValue::Text(String::from(s)) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        assert_eq!(Core::dispatch(idx("jwt_header"), &[t(TOK)]).unwrap(), t(r#"{"alg":"HS256","typ":"JWT"}"#));
        assert_eq!(Core::dispatch(idx("jwt_payload"), &[t(TOK)]).unwrap(), t(r#"{"sub":"1234567890","name":"John Doe","iat":1516239022}"#));
        assert_eq!(Core::dispatch(idx("jwt_payload"), &[t("not-a-jwt")]).unwrap(), NeutralValue::Null);
    }
}
