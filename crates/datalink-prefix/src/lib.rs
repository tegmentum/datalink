//! Database-agnostic function-prefix namespacing.
//!
//! Both the sqlink and ducklink wasm-component hosts namespace an
//! extension's functions SPARQL-style: a function `name` is registered
//! both bare (last-registered-wins) and as a qualified `prefix__name`
//! (always unique, never collides). The prefix + an opaque global
//! "expansion" identity come from the extension's registry entry /
//! manifest; when absent (the deprecation window) both are synthesized.
//!
//! This crate owns the parts of that scheme that are identical across
//! the two hosts:
//!
//!   * the `__` separator, identifier sanitization, and qualified-name
//!     construction ([`PREFIX_SEPARATOR`], [`sanitize_to_identifier`],
//!     [`validate_identifier`], [`qualify`], [`qualified_name`]);
//!   * the resolution decision — registry/manifest entry vs the
//!     deprecation fallback ([`PrefixResolution`], [`resolve_prefix`]);
//!   * the collision / pin MODEL behind a storage trait
//!     ([`PrefixStore`]), so each host backs it its own way: ducklink
//!     in-memory (registry/index.json), sqlink in its `__sqlink_prefix*`
//!     SQLite tables. The bare-name precedence rule ([`PrefixStore::
//!     should_register_bare`]) is a shared default over the store.
//!
//! A ready-to-use in-memory backing ([`InMemoryPrefixStore`]) ships
//! here; it implements the full model (numbered-alias prefix-collision
//! fallback, cross-expansion collision detection, pins) so a host with
//! no durable store can reuse it directly.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::convert::Infallible;

/// Separator between a prefix and a function name in a qualified name.
/// `__` is legal in unquoted SQL identifiers and visually distinct
/// from natural single-underscore names like `uuid_v4`.
pub const PREFIX_SEPARATOR: &str = "__";

/// Bound on auto-fallback numbered prefix-collision resolution. After
/// this many collisions on the same short prefix, the store gives up.
pub const COLLISION_FALLBACK_LIMIT: u32 = 999;

/// Coerce an arbitrary name into a legal unquoted SQL identifier:
/// replace every char outside `[A-Za-z0-9_]` with `_`, and prepend `_`
/// if the result would start with a digit. Never returns empty (a `_`
/// is produced for an empty input). Used for the deprecation fallback
/// prefix and anywhere a host must turn a raw extension name into a
/// usable prefix.
pub fn sanitize_to_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
        if i == 0 && out.starts_with(|c: char| c.is_ascii_digit()) {
            out.insert(0, '_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Validate that `s` is already a legal unquoted SQL identifier head +
/// body: `[A-Za-z_][A-Za-z0-9_]*`. Returns the string unchanged when
/// valid, else `None`. (Hyphens / colons are NOT valid; a caller that
/// wants coercion instead uses [`sanitize_to_identifier`].)
pub fn validate_identifier(s: &str) -> Option<String> {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(s.to_string())
    } else {
        None
    }
}

/// Build the qualified function name `prefix__name` unconditionally.
pub fn qualify(prefix: &str, name: &str) -> String {
    format!("{prefix}{PREFIX_SEPARATOR}{name}")
}

/// Build the qualified name `prefix__name`, or `None` when it should be
/// skipped:
///   * the bare `name` already contains `__` (likely already prefixed —
///     avoid double-prefixing `p__x` into `p__p__x`), or
///   * `prefix` is not a valid identifier.
pub fn qualified_name(prefix: &str, name: &str) -> Option<String> {
    let prefix = validate_identifier(prefix)?;
    if name.contains(PREFIX_SEPARATOR) {
        return None;
    }
    Some(qualify(&prefix, name))
}

/// The outcome of resolving an extension's prefix + expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrefixResolution {
    /// Both prefix and expansion came from the registry/manifest.
    Registered { prefix: String, expansion: String },
    /// The deprecation-window fallback was synthesized (one or both
    /// fields were absent). Hosts warn once when they hit this.
    Fallback { prefix: String, expansion: String },
}

impl PrefixResolution {
    /// True when the deprecation fallback was synthesized.
    pub fn is_fallback(&self) -> bool {
        matches!(self, PrefixResolution::Fallback { .. })
    }
    /// The resolved short prefix.
    pub fn prefix(&self) -> &str {
        match self {
            PrefixResolution::Registered { prefix, .. }
            | PrefixResolution::Fallback { prefix, .. } => prefix,
        }
    }
    /// The resolved opaque expansion identity.
    pub fn expansion(&self) -> &str {
        match self {
            PrefixResolution::Registered { expansion, .. }
            | PrefixResolution::Fallback { expansion, .. } => expansion,
        }
    }
    /// Consume into `(prefix, expansion, is_fallback)`.
    pub fn into_parts(self) -> (String, String, bool) {
        match self {
            PrefixResolution::Registered { prefix, expansion } => (prefix, expansion, false),
            PrefixResolution::Fallback { prefix, expansion } => (prefix, expansion, true),
        }
    }
}

/// Resolve the `(prefix, expansion)` for an extension.
///
///   * Both `preferred_prefix` and `preferred_expansion` present and
///     non-empty → [`PrefixResolution::Registered`] carrying them
///     verbatim (the caller decides whether to further sanitize the
///     prefix).
///   * Otherwise → [`PrefixResolution::Fallback`] with prefix =
///     `sanitize_to_identifier(name)` and expansion =
///     `"{internal_scheme}://{name}"` (e.g. `internal_scheme =
///     "ducklink-internal"` or `"sqlink-internal"`).
pub fn resolve_prefix(
    name: &str,
    preferred_prefix: Option<&str>,
    preferred_expansion: Option<&str>,
    internal_scheme: &str,
) -> PrefixResolution {
    match (preferred_prefix, preferred_expansion) {
        (Some(p), Some(e)) if !p.is_empty() && !e.is_empty() => PrefixResolution::Registered {
            prefix: p.to_string(),
            expansion: e.to_string(),
        },
        _ => PrefixResolution::Fallback {
            prefix: sanitize_to_identifier(name),
            expansion: format!("{internal_scheme}://{name}"),
        },
    }
}

/// One detected function-name collision: an `(name, n_args)` claimed by
/// more than one expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    pub function_name: String,
    pub n_args: i32,
    /// Expansions that registered this `(name, n_args)`, in load order;
    /// the last is the current bare owner under last-wins.
    pub expansions: Vec<String>,
}

/// Storage trait for the prefix-registry collision/pin model. Each host
/// backs this its own way (ducklink in-memory; sqlink SQLite). A host
/// that doesn't need pins/collisions may no-op those methods.
///
/// The bare-name precedence rule [`PrefixStore::should_register_bare`]
/// is a provided default over [`PrefixStore::lookup_pin`] — shared
/// across hosts so the decision isn't reimplemented per repo.
pub trait PrefixStore {
    /// Storage error type. In-memory stores use [`std::convert::Infallible`].
    type Error;

    /// The expansion currently bound to `prefix`, if any.
    fn lookup_expansion(&self, prefix: &str) -> Result<Option<String>, Self::Error>;

    /// Bind `prefix` → `expansion`, applying numbered-alias collision
    /// fallback when `prefix` is already bound to a different expansion.
    /// Returns the prefix actually used (may be `prefix`, `prefix2`, …).
    fn record_prefix(
        &mut self,
        prefix: &str,
        expansion: &str,
        now: i64,
    ) -> Result<String, Self::Error>;

    /// Record a function registration. Returns the OTHER expansions that
    /// already own the same `(function_name, n_args)` — a non-empty
    /// result is a collision the host warns about.
    fn record_function(
        &mut self,
        expansion: &str,
        function_name: &str,
        n_args: i32,
        extension: &str,
        now: i64,
    ) -> Result<Vec<String>, Self::Error>;

    /// The pinned expansion for `(function_name, n_args)`, if an
    /// operator set one. Hosts without pins return `Ok(None)`.
    fn lookup_pin(
        &self,
        function_name: &str,
        n_args: i32,
    ) -> Result<Option<String>, Self::Error>;

    /// Pin `(function_name, n_args)` to `expansion` (operator action).
    /// Hosts without pins may no-op.
    fn pin(
        &mut self,
        function_name: &str,
        n_args: i32,
        expansion: &str,
        now: i64,
    ) -> Result<(), Self::Error>;

    /// All detected collisions. Hosts without collision tracking return
    /// `Ok(vec![])`.
    fn list_collisions(&self) -> Result<Vec<Collision>, Self::Error>;

    /// Whether THIS extension's registration should claim the bare name
    /// `function_name(n_args)`, given the pin state:
    ///   * no pin → yes (bare gets last-wins, the default semantics);
    ///   * pin targets THIS expansion → yes;
    ///   * pin targets a DIFFERENT expansion → no.
    fn should_register_bare(
        &self,
        function_name: &str,
        n_args: i32,
        my_expansion: &str,
    ) -> Result<bool, Self::Error> {
        Ok(match self.lookup_pin(function_name, n_args)? {
            None => true,
            Some(pinned) => pinned == my_expansion,
        })
    }
}

/// A complete in-memory [`PrefixStore`]. Implements the full model:
/// numbered-alias prefix-collision fallback, cross-expansion function
/// collision detection, and pins. Suitable for a host with no durable
/// store (ducklink's registry-backed lookups layer on top of this).
#[derive(Default, Debug)]
pub struct InMemoryPrefixStore {
    /// prefix -> expansion.
    prefixes: HashMap<String, String>,
    /// (function_name, n_args) -> expansions in load order.
    functions: HashMap<(String, i32), Vec<String>>,
    /// (function_name, n_args) -> pinned expansion.
    pins: HashMap<(String, i32), String>,
}

impl InMemoryPrefixStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PrefixStore for InMemoryPrefixStore {
    type Error = Infallible;

    fn lookup_expansion(&self, prefix: &str) -> Result<Option<String>, Self::Error> {
        Ok(self.prefixes.get(prefix).cloned())
    }

    fn record_prefix(
        &mut self,
        prefix: &str,
        expansion: &str,
        _now: i64,
    ) -> Result<String, Self::Error> {
        // Idempotent reload, or unbound slot.
        match self.prefixes.get(prefix) {
            Some(e) if e == expansion => return Ok(prefix.to_string()),
            None => {
                self.prefixes.insert(prefix.to_string(), expansion.to_string());
                return Ok(prefix.to_string());
            }
            Some(_) => {}
        }
        // Collision: walk prefix2..prefixN for a free / matching slot.
        for n in 2..=COLLISION_FALLBACK_LIMIT {
            let alias = format!("{prefix}{n}");
            match self.prefixes.get(&alias) {
                Some(e) if e == expansion => return Ok(alias),
                Some(_) => continue,
                None => {
                    self.prefixes.insert(alias.clone(), expansion.to_string());
                    return Ok(alias);
                }
            }
        }
        // Exhausted: fall back to the requested prefix (last-wins).
        Ok(prefix.to_string())
    }

    fn record_function(
        &mut self,
        expansion: &str,
        function_name: &str,
        n_args: i32,
        _extension: &str,
        _now: i64,
    ) -> Result<Vec<String>, Self::Error> {
        let key = (function_name.to_string(), n_args);
        let entry = self.functions.entry(key).or_default();
        let others: Vec<String> = entry.iter().filter(|e| *e != expansion).cloned().collect();
        if !entry.iter().any(|e| e == expansion) {
            entry.push(expansion.to_string());
        }
        Ok(others)
    }

    fn lookup_pin(
        &self,
        function_name: &str,
        n_args: i32,
    ) -> Result<Option<String>, Self::Error> {
        Ok(self.pins.get(&(function_name.to_string(), n_args)).cloned())
    }

    fn pin(
        &mut self,
        function_name: &str,
        n_args: i32,
        expansion: &str,
        _now: i64,
    ) -> Result<(), Self::Error> {
        self.pins
            .insert((function_name.to_string(), n_args), expansion.to_string());
        Ok(())
    }

    fn list_collisions(&self) -> Result<Vec<Collision>, Self::Error> {
        Ok(self
            .functions
            .iter()
            .filter(|(_, exps)| exps.len() > 1)
            .map(|((name, n_args), exps)| Collision {
                function_name: name.clone(),
                n_args: *n_args,
                expansions: exps.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_coerces_to_identifier() {
        assert_eq!(sanitize_to_identifier("iban-component"), "iban_component");
        assert_eq!(sanitize_to_identifier("jsonfns"), "jsonfns");
        assert_eq!(sanitize_to_identifier("123x"), "_123x");
        assert_eq!(sanitize_to_identifier("foo.bar"), "foo_bar");
        assert_eq!(sanitize_to_identifier("foo:bar"), "foo_bar");
        assert_eq!(sanitize_to_identifier(""), "_");
    }

    #[test]
    fn validate_accepts_only_valid_identifiers() {
        assert_eq!(validate_identifier("jsonfns").as_deref(), Some("jsonfns"));
        assert_eq!(validate_identifier("_foo1").as_deref(), Some("_foo1"));
        assert_eq!(validate_identifier("ST_2").as_deref(), Some("ST_2"));
        assert_eq!(validate_identifier("2foo"), None);
        assert_eq!(validate_identifier("iban-component"), None);
        assert_eq!(validate_identifier("foaf:name"), None);
        assert_eq!(validate_identifier(""), None);
    }

    #[test]
    fn qualify_uses_double_underscore() {
        assert_eq!(qualify("foaf", "name"), "foaf__name");
        assert_eq!(qualify("uuid", "v4"), "uuid__v4");
    }

    #[test]
    fn qualified_name_basic_and_skips() {
        assert_eq!(
            qualified_name("jsonfns", "json_valid").as_deref(),
            Some("jsonfns__json_valid")
        );
        // already-prefixed bare name -> skip to avoid double-prefixing
        assert_eq!(qualified_name("jsonfns", "jsonfns__json_valid"), None);
        // invalid prefix -> skip
        assert_eq!(qualified_name("bad-prefix", "x"), None);
    }

    #[test]
    fn resolve_uses_manifest_when_both_present() {
        let r = resolve_prefix("uuid", Some("uuid"), Some("urn:uuid"), "sqlink-internal");
        assert_eq!(r, PrefixResolution::Registered { prefix: "uuid".into(), expansion: "urn:uuid".into() });
        assert!(!r.is_fallback());
        assert_eq!(r.into_parts(), ("uuid".into(), "urn:uuid".into(), false));
    }

    #[test]
    fn resolve_synthesizes_when_missing() {
        let r = resolve_prefix("foo-bar", None, None, "sqlink-internal");
        assert!(r.is_fallback());
        assert_eq!(r.prefix(), "foo_bar");
        assert_eq!(r.expansion(), "sqlink-internal://foo-bar");
    }

    #[test]
    fn resolve_synthesizes_when_one_missing() {
        let r = resolve_prefix("uuid", Some("uuid"), None, "ducklink-internal");
        assert!(r.is_fallback());
        assert_eq!(r.prefix(), "uuid");
        assert_eq!(r.expansion(), "ducklink-internal://uuid");
    }

    #[test]
    fn store_prefix_collision_numbered_fallback() {
        let mut s = InMemoryPrefixStore::new();
        assert_eq!(s.record_prefix("foaf", "exp-a", 1).unwrap(), "foaf");
        // idempotent reload
        assert_eq!(s.record_prefix("foaf", "exp-a", 2).unwrap(), "foaf");
        // different expansion -> numbered alias
        assert_eq!(s.record_prefix("foaf", "exp-b", 3).unwrap(), "foaf2");
        assert_eq!(s.record_prefix("foaf", "exp-c", 4).unwrap(), "foaf3");
        // reuse the existing fallback slot for exp-b
        assert_eq!(s.record_prefix("foaf", "exp-b", 5).unwrap(), "foaf2");
    }

    #[test]
    fn store_function_collision_detection() {
        let mut s = InMemoryPrefixStore::new();
        assert!(s.record_function("exp-a", "concat", 2, "exta", 1).unwrap().is_empty());
        assert_eq!(s.record_function("exp-b", "concat", 2, "extb", 1).unwrap(), vec!["exp-a"]);
        // same expansion re-registering doesn't see itself
        assert!(s.record_function("exp-a", "concat", 2, "exta", 2).unwrap() == vec!["exp-b"]);
        let cols = s.list_collisions().unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].function_name, "concat");
    }

    #[test]
    fn store_should_register_bare_respects_pin() {
        let mut s = InMemoryPrefixStore::new();
        assert!(s.should_register_bare("concat", 2, "any").unwrap());
        s.pin("concat", 2, "exp-a", 1).unwrap();
        assert!(s.should_register_bare("concat", 2, "exp-a").unwrap());
        assert!(!s.should_register_bare("concat", 2, "exp-b").unwrap());
    }
}
