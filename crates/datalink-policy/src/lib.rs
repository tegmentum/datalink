//! Canonical, database-agnostic Rust types for per-extension
//! capability grants.
//!
//! This crate is the single source of truth for `Capability`,
//! `HttpPolicy`, `DnsPolicy`, `Policy`, and `PolicyError`. The
//! sqlink and ducklink wasm-component hosts both depend on it so a
//! `Policy` value constructed once is portable across deployment
//! modes and across hosts.
//!
//! The crate intentionally has no `wasmtime`, `wit-bindgen-rt`, or
//! DB-runtime dependency. Bindgen-generated capability/policy types
//! are crate-local to each consumer — they would otherwise force this
//! crate to pick one runtime — so the conversion from a runtime's WIT
//! types lives at each consumer instead. The shape of that conversion
//! is identical everywhere: walk the WIT capability list, map each
//! variant to `Capability`, copy the numeric knobs across.
//!
//! Both a coarse host (one allow/deny bit per extension) and a
//! fine-grained host (per-extension, per-host-pattern grants) can be
//! expressed against these types: the coarse host compiles its single
//! switch into an allow-all / deny-all `Policy`; the fine-grained
//! host populates the per-capability grant set and the HTTP/DNS
//! allowlists directly.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]

use std::collections::HashSet;

/// One capability per gated host-imported interface.
///
/// The set is a union across the hosts that consume this crate. A
/// host grants only the capabilities its world actually exposes; the
/// rest are simply never granted (and never declared by that host's
/// extensions). Utility capabilities (`text`, `hashing`, `encoding`,
/// `random`) are represented for transparency even though hosts will
/// usually grant them by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// The host's SQL-programming-interface surface (query/exec).
    Spi,
    Prepared,
    Transaction,
    Schema,
    State,
    Cache,
    Random,
    Text,
    Hashing,
    Encoding,
    Http,
    Dns,
    /// Raw WAL-frame access. Substrate for a WAL-archive extension;
    /// gated separately from `Spi` so an extension can read WAL bytes
    /// without holding the full SQL surface (and vice versa).
    WalFrames,
    /// S3-compatible object storage. Endpoint URL + credentials are
    /// runtime arguments to each call rather than policy fields — the
    /// operator's grant is purely an allow-the-surface bit.
    S3,
    /// Host-resident cargo build. No rich policy — the operator's
    /// grant is purely an allow-the-surface bit; browser deployments
    /// fail-closed at the WIT contract.
    SpawnBuild,
    /// Bundle storage: read/write access to the host's
    /// content-addressed bundle registry. Pairs with `SpawnBuild` for
    /// the with-build path; metadata-only operations need only this.
    Bundles,
}

/// Outbound HTTP policy. The host's HTTP impl consults this before
/// letting an extension hit the network.
///
/// Method names are unstructured strings (e.g. `"GET"`, `"POST"`)
/// rather than a `Method` enum so the type stays bindgen-free.
/// Consumers that use a runtime-specific `Method` enum should match
/// by string before/after the boundary.
#[derive(Debug, Clone, Default)]
pub struct HttpPolicy {
    /// Allowlist of hostnames. Entries may use a leading `*.` for a
    /// wildcard suffix (e.g. `*.example.com` matches
    /// `api.example.com` but not `example.com` itself). Empty = no
    /// hosts permitted.
    pub allowed_hosts: Vec<String>,
    /// Optional method allowlist. `None` = any method allowed.
    pub allowed_methods: Option<Vec<String>>,
    /// Cap on response body bytes returned to the extension.
    pub max_body_bytes: Option<u64>,
    /// Per-request wall-clock timeout. Overrides any timeout the
    /// extension passes.
    pub timeout_ms: Option<u32>,
}

impl HttpPolicy {
    /// True if `host` matches an allowlist entry (literal or
    /// `*.suffix` wildcard).
    pub fn allows(&self, host: &str) -> bool {
        for entry in &self.allowed_hosts {
            if let Some(suffix) = entry.strip_prefix("*.") {
                if host.ends_with(suffix) && host.len() > suffix.len() {
                    // Wildcard requires at least one character before
                    // the suffix.
                    return true;
                }
            } else if entry == host {
                return true;
            }
        }
        false
    }

    /// `Ok(())` if allowed; `Err(PolicyError::HostNotAllowed)`
    /// otherwise. Convenience wrapper over [`Self::allows`].
    pub fn check_host(&self, host: &str) -> Result<(), PolicyError> {
        if self.allows(host) {
            Ok(())
        } else {
            Err(PolicyError::HostNotAllowed(host.to_string()))
        }
    }

    /// `Ok(())` if `method` is in the allowlist (or no list set);
    /// `Err(PolicyError::MethodNotAllowed)` otherwise.
    pub fn check_method(&self, method: &str) -> Result<(), PolicyError> {
        match &self.allowed_methods {
            None => Ok(()),
            Some(list) if list.iter().any(|m| m.eq_ignore_ascii_case(method)) => Ok(()),
            Some(_) => Err(PolicyError::MethodNotAllowed(method.to_string())),
        }
    }
}

/// Outbound DNS policy. The host's DNS-resolve impl consults this
/// before letting an extension issue a lookup.
#[derive(Debug, Clone, Default)]
pub struct DnsPolicy {
    /// Allowlist of domains. Entries may use a leading `*.` for a
    /// wildcard suffix (same semantics as [`HttpPolicy::allowed_hosts`]).
    /// Empty list = no domains permitted.
    pub allowed_domains: Vec<String>,
    /// Per-request wall-clock timeout. Overrides any timeout the
    /// extension passes.
    pub timeout_ms: Option<u32>,
}

impl DnsPolicy {
    /// True if `name` matches an allowlist entry (literal or `*.suffix`
    /// wildcard). Strips a trailing dot from `name` before matching so
    /// `example.com.` and `example.com` compare equal.
    pub fn allows(&self, name: &str) -> bool {
        let name = name.strip_suffix('.').unwrap_or(name);
        for entry in &self.allowed_domains {
            if let Some(suffix) = entry.strip_prefix("*.") {
                if name.ends_with(suffix) && name.len() > suffix.len() {
                    return true;
                }
            } else if entry == name {
                return true;
            }
        }
        false
    }

    /// `Ok(())` if allowed; `Err(PolicyError::DnsDomainNotAllowed)` otherwise.
    pub fn check_domain(&self, name: &str) -> Result<(), PolicyError> {
        if self.allows(name) {
            Ok(())
        } else {
            Err(PolicyError::DnsDomainNotAllowed(name.to_string()))
        }
    }
}

/// Full per-extension policy: a precomputed grant set plus the
/// resource knobs and the HTTP/DNS allowlists.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    granted: HashSet<Capability>,
    pub http: Option<HttpPolicy>,
    pub dns: Option<DnsPolicy>,
    pub fuel_per_call: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub epoch_deadline_ms: Option<u64>,
}

impl Policy {
    /// A Policy that grants nothing. Fail-closed default.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// A Policy that grants every capability and attaches HTTP + DNS
    /// allowlists that permit any host/domain (a `*` wildcard entry).
    /// Used by a coarse host to compile its single "network on" switch
    /// into the canonical type model.
    pub fn allow_all() -> Self {
        Self::default()
            .with_grants([
                Capability::Spi,
                Capability::Prepared,
                Capability::Transaction,
                Capability::Schema,
                Capability::State,
                Capability::Cache,
                Capability::Random,
                Capability::Text,
                Capability::Hashing,
                Capability::Encoding,
                Capability::Http,
                Capability::Dns,
                Capability::WalFrames,
                Capability::S3,
                Capability::SpawnBuild,
                Capability::Bundles,
            ])
            .with_http(HttpPolicy {
                allowed_hosts: vec!["*.".to_string()],
                ..Default::default()
            })
            .with_dns(DnsPolicy {
                allowed_domains: vec!["*.".to_string()],
                ..Default::default()
            })
    }

    /// Builder: grant the listed capabilities.
    pub fn with_grants(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.granted.extend(caps);
        self
    }

    /// Builder: attach an HTTP policy. Required when `Capability::Http`
    /// is in the grant list — see [`Self::validate`].
    pub fn with_http(mut self, http: HttpPolicy) -> Self {
        self.http = Some(http);
        self
    }

    /// Builder: attach a DNS policy. Required when `Capability::Dns`
    /// is in the grant list — see [`Self::validate`].
    pub fn with_dns(mut self, dns: DnsPolicy) -> Self {
        self.dns = Some(dns);
        self
    }

    /// Builder: per-call fuel budget (wasmtime fuel reset before
    /// each guest invocation). `None` = unlimited.
    pub fn with_fuel_per_call(mut self, fuel: u64) -> Self {
        self.fuel_per_call = Some(fuel);
        self
    }

    /// Builder: hard cap on the extension's linear memory.
    pub fn with_memory_limit_bytes(mut self, n: u64) -> Self {
        self.memory_limit_bytes = Some(n);
        self
    }

    /// Builder: wall-clock deadline per guest invocation
    /// (milliseconds). Best-effort on runtimes that lack epoch
    /// interruption.
    pub fn with_epoch_deadline_ms(mut self, ms: u64) -> Self {
        self.epoch_deadline_ms = Some(ms);
        self
    }

    /// True if `cap` is in the grant list.
    pub fn is_granted(&self, cap: Capability) -> bool {
        self.granted.contains(&cap)
    }

    /// Return capabilities in `declared` that are NOT granted.
    /// Empty result ⇒ manifest is satisfiable.
    pub fn missing<'a>(&self, declared: &'a [Capability]) -> Vec<&'a Capability> {
        declared
            .iter()
            .filter(|c| !self.granted.contains(c))
            .collect()
    }

    /// `Ok(())` if every entry of `declared` is granted; otherwise
    /// `Err(PolicyError::CapabilityNotGranted(<first missing>))`.
    pub fn check_manifest(&self, declared: &[Capability]) -> Result<(), PolicyError> {
        for c in declared {
            if !self.granted.contains(c) {
                return Err(PolicyError::CapabilityNotGranted(*c));
            }
        }
        Ok(())
    }

    /// Cross-field validation. Run after construction to catch
    /// internally-inconsistent policies (e.g., http capability
    /// granted but no HttpPolicy attached).
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.granted.contains(&Capability::Http) && self.http.is_none() {
            return Err(PolicyError::MissingHttpPolicy);
        }
        if self.granted.contains(&Capability::Dns) && self.dns.is_none() {
            return Err(PolicyError::MissingDnsPolicy);
        }
        Ok(())
    }
}

/// Errors raised by the policy layer.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyError {
    /// Extension declared a capability the host didn't grant.
    CapabilityNotGranted(Capability),
    /// Extension tried to use a capability it never declared.
    CapabilityNotDeclared(Capability),
    /// HTTP request to a host not on the allowlist.
    HostNotAllowed(String),
    /// HTTP request used a method outside `allowed_methods`.
    MethodNotAllowed(String),
    /// `grant` includes `Http` but `http_policy` was `None`.
    MissingHttpPolicy,
    /// DNS query targeted a domain outside `dns_policy.allowed_domains`.
    DnsDomainNotAllowed(String),
    /// `grant` includes `Dns` but `dns_policy` was `None`.
    MissingDnsPolicy,
    /// Per-call fuel budget exhausted.
    FuelExhausted,
    /// Memory cap reached.
    MemoryLimitExceeded,
    /// Wall-clock deadline reached.
    EpochDeadlineExceeded,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityNotGranted(c) => write!(f, "capability {c:?} not granted"),
            Self::CapabilityNotDeclared(c) => write!(f, "capability {c:?} not declared"),
            Self::HostNotAllowed(h) => write!(f, "host {h:?} not on http allowlist"),
            Self::MethodNotAllowed(m) => write!(f, "method {m:?} not on http allowlist"),
            Self::MissingHttpPolicy => write!(f, "grant includes http but no http policy attached"),
            Self::DnsDomainNotAllowed(d) => write!(f, "domain {d:?} not on dns allowlist"),
            Self::MissingDnsPolicy => write!(f, "grant includes dns but no dns policy attached"),
            Self::FuelExhausted => write!(f, "per-call fuel exhausted"),
            Self::MemoryLimitExceeded => write!(f, "memory limit exceeded"),
            Self::EpochDeadlineExceeded => write!(f, "epoch deadline exceeded"),
        }
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_all_grants_nothing() {
        let p = Policy::deny_all();
        for c in [Capability::Spi, Capability::Http, Capability::Random, Capability::S3, Capability::SpawnBuild] {
            assert!(!p.is_granted(c));
        }
    }

    #[test]
    fn allow_all_grants_network() {
        let p = Policy::allow_all();
        assert!(p.is_granted(Capability::Http));
        assert!(p.is_granted(Capability::Dns));
        // The wildcard allowlists permit any host/domain.
        assert!(p.http.as_ref().unwrap().allows("api.example.com"));
        assert!(p.dns.as_ref().unwrap().allows("api.example.com"));
        // allow_all is internally consistent (http/dns policies attached).
        assert!(p.validate().is_ok());
    }

    #[test]
    fn grants_set_membership() {
        let p = Policy::deny_all().with_grants([Capability::Spi, Capability::Http]);
        assert!(p.is_granted(Capability::Spi));
        assert!(p.is_granted(Capability::Http));
        assert!(!p.is_granted(Capability::State));
    }

    #[test]
    fn missing_returns_only_ungranted() {
        let p = Policy::deny_all().with_grants([Capability::Spi]);
        let declared = [Capability::Spi, Capability::State, Capability::Http];
        let missing = p.missing(&declared);
        assert_eq!(missing, vec![&Capability::State, &Capability::Http]);
    }

    #[test]
    fn check_manifest_succeeds_when_subset() {
        let p = Policy::deny_all().with_grants([Capability::Spi, Capability::Random]);
        assert!(p.check_manifest(&[Capability::Spi]).is_ok());
        assert!(p.check_manifest(&[]).is_ok());
    }

    #[test]
    fn check_manifest_errors_with_first_missing() {
        let p = Policy::deny_all().with_grants([Capability::Spi]);
        assert_eq!(
            p.check_manifest(&[Capability::Spi, Capability::Http]),
            Err(PolicyError::CapabilityNotGranted(Capability::Http))
        );
    }

    #[test]
    fn check_manifest_ignores_optional_without_grant() {
        let p = Policy::deny_all().with_grants([Capability::Bundles]);
        assert!(p.check_manifest(&[Capability::Bundles]).is_ok());
        assert!(!p.is_granted(Capability::SpawnBuild));
    }

    #[test]
    fn http_allows_exact_host() {
        let h = HttpPolicy {
            allowed_hosts: vec!["api.example.com".into()],
            ..Default::default()
        };
        assert!(h.allows("api.example.com"));
        assert!(!h.allows("api.other.com"));
    }

    #[test]
    fn http_wildcard_suffix() {
        let h = HttpPolicy {
            allowed_hosts: vec!["*.example.com".into()],
            ..Default::default()
        };
        assert!(h.allows("api.example.com"));
        assert!(h.allows("a.b.example.com"));
        // Wildcard requires at least one character before the suffix.
        assert!(!h.allows("example.com"));
        assert!(!h.allows("other.com"));
    }

    #[test]
    fn http_method_check() {
        let h = HttpPolicy {
            allowed_hosts: vec!["x.com".into()],
            allowed_methods: Some(vec!["GET".into(), "POST".into()]),
            ..Default::default()
        };
        assert!(h.check_method("GET").is_ok());
        assert!(h.check_method("get").is_ok()); // case-insensitive
        assert!(h.check_method("DELETE").is_err());
    }

    #[test]
    fn validate_catches_http_without_policy() {
        let p = Policy::deny_all().with_grants([Capability::Http]);
        assert_eq!(p.validate(), Err(PolicyError::MissingHttpPolicy));
    }
}
