//! Neutral core for the `netquack` extension — public-suffix-aware
//! URL/domain parsing (via the `psl` crate, Mozilla Public Suffix List) —
//! written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table below.
//!
//!   * `registrable_domain(host_or_url) -> text` (eTLD+1)
//!   * `public_suffix(host_or_url) -> text` (effective TLD)
//!   * `subdomain(host_or_url) -> text` (labels left of registrable; '' if none)
//!   * `domain_label(host_or_url) -> text` (registrable name without its suffix)
//!
//! NULL / unparseable input -> NULL. Never panics.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// Logic, byte-for-byte the pre-pullup algorithm (DB-agnostic).
pub mod logic {
    use alloc::string::{String, ToString};

    /// Extracts a bare host from a bare host OR a full URL. Strips scheme,
    /// userinfo, port, path/query/fragment, trailing dot, and lowercases.
    pub fn extract_host(input: &str) -> Option<String> {
        let mut s = input.trim();
        if let Some(pos) = s.find("://") {
            s = &s[pos + 3..];
        } else if let Some(pos) = s.find(':') {
            let scheme = &s[..pos];
            if !scheme.is_empty()
                && scheme.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
                && !s[pos + 1..]
                    .chars()
                    .take_while(|c| *c != '/')
                    .all(|c| c.is_ascii_digit())
            {
                s = &s[pos + 1..];
            }
        }
        s = s.trim_start_matches('/');
        let authority = s.split(|c| c == '/' || c == '?' || c == '#').next().unwrap_or("");
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        let host = if let Some(stripped) = hostport.strip_prefix('[') {
            stripped.split(']').next().unwrap_or("")
        } else {
            hostport.rsplit(':').last().unwrap_or(hostport)
        };
        let host = host.trim().trim_end_matches('.');
        if host.is_empty() {
            return None;
        }
        Some(host.to_ascii_lowercase())
    }

    /// (registrable_domain, public_suffix, host) for a host/URL. `None` when
    /// the host has no eTLD+1 (bare TLD, IP literal, unparseable input).
    pub fn parse(input: &str) -> Option<(String, String, String)> {
        let host = extract_host(input)?;
        let domain = psl::domain(host.as_bytes())?;
        let suffix = core::str::from_utf8(domain.suffix().as_bytes()).ok()?.to_string();
        let registrable = core::str::from_utf8(domain.as_bytes()).ok()?.to_string();
        Some((registrable, suffix, host))
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "netquack";
    version = env!("CARGO_PKG_VERSION");

    scalar registrable_domain(text) -> text [propagate, deterministic] = |args| {
        let input = args.arg_text(0, "registrable_domain")?;
        Ok(match logic::parse(&input) {
            Some((reg, _, _)) => NeutralValue::Text(reg),
            None => NeutralValue::Null,
        })
    };

    scalar public_suffix(text) -> text [propagate, deterministic] = |args| {
        let input = args.arg_text(0, "public_suffix")?;
        Ok(match logic::parse(&input) {
            Some((_, suf, _)) => NeutralValue::Text(suf),
            None => NeutralValue::Null,
        })
    };

    scalar subdomain(text) -> text [propagate, deterministic] = |args| {
        let input = args.arg_text(0, "subdomain")?;
        Ok(match logic::parse(&input) {
            Some((reg, _, host)) => {
                let sub = host.strip_suffix(&reg).map(|p| p.trim_end_matches('.')).unwrap_or("");
                NeutralValue::Text(alloc::string::String::from(sub))
            }
            None => NeutralValue::Null,
        })
    };

    scalar domain_label(text) -> text [propagate, deterministic] = |args| {
        let input = args.arg_text(0, "domain_label")?;
        Ok(match logic::parse(&input) {
            Some((reg, suf, _)) => {
                let label = reg.strip_suffix(&suf).map(|p| p.trim_end_matches('.')).unwrap_or(&reg);
                NeutralValue::Text(alloc::string::String::from(label))
            }
            None => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;
    use std::vec;

    fn t(s: &str) -> NeutralValue {
        NeutralValue::Text(String::from(s))
    }

    #[test]
    fn parsing() {
        assert_eq!(
            Core::dispatch(0, &[t("https://a.b.example.co.uk/x")]).unwrap(),
            t("example.co.uk")
        );
        assert_eq!(Core::dispatch(1, &[t("https://a.b.example.co.uk/x")]).unwrap(), t("co.uk"));
        assert_eq!(Core::dispatch(2, &[t("https://a.b.example.co.uk/x")]).unwrap(), t("a.b"));
        assert_eq!(Core::dispatch(3, &[t("https://a.b.example.co.uk/x")]).unwrap(), t("example"));
        assert_eq!(Core::dispatch(2, &[t("example.com")]).unwrap(), t(""));
        assert_eq!(Core::dispatch(0, &[t("co.uk")]).unwrap(), NeutralValue::Null);
    }
}
