//! Neutral core for the `secp256k1` extension — ECDSA over the secp256k1
//! curve via `k256` — written ONCE. The per-DB shim is generated from
//! the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `secp256k1_pubkey(privkey blob) -> blob`  (33-byte compressed)
//!   * `secp256k1_sign(msg_hash, privkey) -> blob`  (64-byte compact,
//!                                                   RFC 6979 deterministic)
//!   * `secp256k1_verify(msg_hash, sig, pubkey) -> boolean`
//!
//! Malformed / NULL -> NULL. The surface is identical in both ports
//! (zero drift).

extern crate alloc;

use alloc::vec::Vec;
use datalink_extcore::NeutralValue;

/// ECDSA primitives (DB-agnostic).
pub mod logic {
    use super::Vec;
    use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
    use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

    /// 32-byte privkey -> 33-byte compressed pubkey, or None.
    pub fn pubkey(priv_bytes: &[u8]) -> Option<Vec<u8>> {
        if priv_bytes.len() != 32 {
            return None;
        }
        let sk = SigningKey::from_bytes(priv_bytes.into()).ok()?;
        let vk = VerifyingKey::from(&sk);
        Some(vk.to_encoded_point(true).as_bytes().to_vec())
    }

    /// Deterministic (RFC 6979) sign of a 32-byte prehash -> 64-byte
    /// compact sig, or None.
    pub fn sign(hash: &[u8], priv_bytes: &[u8]) -> Option<Vec<u8>> {
        if hash.len() != 32 || priv_bytes.len() != 32 {
            return None;
        }
        let sk = SigningKey::from_bytes(priv_bytes.into()).ok()?;
        let sig: Signature = sk.sign_prehash(hash).ok()?;
        let sig = sig.normalize_s().unwrap_or(sig);
        Some(sig.to_bytes().to_vec())
    }

    /// Verify a 64-byte compact sig over a 32-byte prehash with a
    /// 33/65-byte pubkey. None on malformed input; Some(bool) otherwise.
    pub fn verify(hash: &[u8], sig_bytes: &[u8], pub_bytes: &[u8]) -> Option<bool> {
        if hash.len() != 32 || sig_bytes.len() != 64 {
            return None;
        }
        let vk = VerifyingKey::from_sec1_bytes(pub_bytes).ok()?;
        let sig = Signature::from_slice(sig_bytes).ok()?;
        let sig = sig.normalize_s().unwrap_or(sig);
        Some(vk.verify_prehash(hash, &sig).is_ok())
    }
}

fn opt_blob(o: Option<Vec<u8>>) -> NeutralValue {
    match o {
        Some(b) => NeutralValue::Blob(b),
        None => NeutralValue::Null,
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "secp256k1";
    version = env!("CARGO_PKG_VERSION");

    scalar secp256k1_pubkey(blob) -> blob [propagate, deterministic] = |args| {
        Ok(opt_blob(logic::pubkey(&args.arg_blob(0, "secp256k1_pubkey")?)))
    };
    scalar secp256k1_sign(blob, blob) -> blob [propagate, deterministic] = |args| {
        let hash = args.arg_blob(0, "secp256k1_sign")?;
        let pk = args.arg_blob(1, "secp256k1_sign")?;
        Ok(opt_blob(logic::sign(&hash, &pk)))
    };
    scalar secp256k1_verify(blob, blob, blob) -> boolean [propagate, deterministic] = |args| {
        let hash = args.arg_blob(0, "secp256k1_verify")?;
        let sig = args.arg_blob(1, "secp256k1_verify")?;
        let pk = args.arg_blob(2, "secp256k1_verify")?;
        Ok(match logic::verify(&hash, &sig, &pk) {
            Some(ok) => NeutralValue::Boolean(ok),
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;
    fn b(v: Vec<u8>) -> NeutralValue { NeutralValue::Blob(v) }
    fn idx(n: &str) -> usize { Core::DECLS.iter().position(|d| d.name == n).unwrap() }

    #[test]
    fn matches_baseline() {
        let pk = vec![1u8; 32];
        let hash = vec![0x42u8; 32];
        let pub_v = match Core::dispatch(idx("secp256k1_pubkey"), &[b(pk.clone())]).unwrap() {
            NeutralValue::Blob(v) => v,
            other => panic!("expected blob, got {other:?}"),
        };
        assert_eq!(pub_v.len(), 33);
        let sig_v = match Core::dispatch(idx("secp256k1_sign"), &[b(hash.clone()), b(pk.clone())]).unwrap() {
            NeutralValue::Blob(v) => v,
            other => panic!("expected blob, got {other:?}"),
        };
        assert_eq!(sig_v.len(), 64);
        assert_eq!(Core::dispatch(idx("secp256k1_verify"), &[b(hash), b(sig_v), b(pub_v)]).unwrap(), NeutralValue::Boolean(true));
        assert_eq!(Core::dispatch(idx("secp256k1_pubkey"), &[b(vec![1u8, 2u8])]).unwrap(), NeutralValue::Null);
    }
}
