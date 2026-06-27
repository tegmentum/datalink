//! datalink-contract — the shared runtime WIT contract-version load guard for
//! the `sqlink` and `ducklink` wasm-component hosts.
//!
//! Both hosts load wasm **components** that import a versioned WIT package
//! (`duckdb:extension@MAJOR.minor.patch` for ducklink, `sqlink:wasm@MAJOR...`
//! for sqlink). When the canonical WIT moves to a new MAJOR, an old component
//! compiled against the previous major is ABI-incompatible: instantiating it
//! would either trap with a cryptic wasmtime type-mismatch, or — worse, for the
//! legacy *unversioned* case where the import names happen to still line up —
//! silently marshal corrupted values (a rich-types bump can shift enum
//! discriminants without changing the import names).
//!
//! This crate is the runtime-observable guard that both hosts call BEFORE
//! instantiating a component: it introspects the component's imported package
//! `@MAJOR` and rejects any component whose major differs from the host's (or
//! that imports the package unversioned/legacy) with a clear, actionable error.
//!
//! It is generic over the package name so the same logic serves both contracts:
//! pass `package = "duckdb:extension"` (ducklink) or `package = "sqlink:wasm"`
//! (sqlink), and the host's contract major.
//!
//! NOTE: the AUTHORITATIVE content-addressed contract identity (the witcanon
//! `CONTRACT_DIGEST`, computed at build time over the canonical WIT bytes) is
//! SEPARATE and stays per-repo — the runtime cannot recompute a *loaded*
//! component's WIT digest, so this `@MAJOR` guard is the runtime PROXY for it.
//! This crate is only that runtime proxy; the digest is enforced at
//! catalog-verify time in each repo.

use anyhow::Result;
use wasmtime::component::Component;
use wasmtime::Engine;

/// The WIT contract MAJOR a component targets, read from its imported package
/// ids, for the given `package` (e.g. `"duckdb:extension"` or `"sqlink:wasm"`).
///
/// Returns:
///   - `Some(major)` if it imports `<package>/...@MAJOR.minor.patch`
///   - `None` if it imports the package UNVERSIONED (legacy, pre-versioning) —
///     or imports nothing from `<package>` at all (in practice every loadable
///     extension imports at least one interface from its contract package).
///
/// The introspection uses `component.component_type().imports(engine)`, whose
/// instance names look like `duckdb:extension/runtime@2.0.0` or, for a legacy
/// component, `duckdb:extension/runtime` (no version).
pub fn component_contract_major(
    engine: &Engine,
    component: &Component,
    package: &str,
) -> Option<u64> {
    for (name, _) in component.component_type().imports(engine) {
        // `name` is an instance import like `<package>/<iface>@MAJOR.minor.patch`
        // or, for a legacy/unversioned component, `<package>/<iface>`.
        let pkg = name.split('/').next().unwrap_or(name);
        if pkg.starts_with(package) {
            return match name.rsplit_once('@') {
                Some((_, ver)) => ver.split('.').next().and_then(|m| m.parse::<u64>().ok()),
                None => None, // unversioned -> legacy
            };
        }
    }
    None
}

/// The contract `(major, minor)` a component targets, read from its imported
/// package ids, for the given `package` (e.g. `"duckdb:extension"`).
///
/// Returns:
///   - `Some((major, minor))` if it imports `<package>/...@MAJOR.MINOR.x`; `minor`
///     is the MAX minor across the package's interface imports (a 2.1 component
///     imports the new `@2.1.x` interfaces -> minor 1; an existing 2.0 component
///     imports everything `@2.0.x` -> minor 0).
///   - `None` if it imports the package UNVERSIONED (legacy pre-versioning) — or
///     imports nothing from `<package>` at all.
///
/// This is the MINOR-granular companion to [`component_contract_major`]: MINORs
/// are ADDITIVE, so a host at `MAJOR.minor` can load any component built at
/// `MAJOR.k` for `k <= minor`, but NOT one built at a higher minor (it imports
/// interfaces the host does not provide). The introspection mirrors
/// [`component_contract_major`]: scan `component_type().imports`, whose instance
/// names look like `duckdb:extension/runtime@2.1.0`.
pub fn component_contract_version(
    engine: &Engine,
    component: &Component,
    package: &str,
) -> Option<(u64, u64)> {
    let mut found: Option<(u64, u64)> = None;
    for (name, _) in component.component_type().imports(engine) {
        let pkg = name.split('/').next().unwrap_or(name);
        if !pkg.starts_with(package) {
            continue;
        }
        let ver = match name.rsplit_once('@') {
            Some((_, ver)) => ver,
            None => return None, // unversioned -> legacy
        };
        let mut parts = ver.split('.');
        let major = parts.next().and_then(|m| m.parse::<u64>().ok());
        let minor = parts.next().and_then(|m| m.parse::<u64>().ok()).unwrap_or(0);
        if let Some(major) = major {
            found = match found {
                // Keep the highest (major, minor) seen for the contract package.
                Some(cur) if cur >= (major, minor) => Some(cur),
                _ => Some((major, minor)),
            };
        }
    }
    found
}

/// Loader pre-check: given the `imported_major` a component targets (as returned
/// by [`component_contract_major`]) and the `host_major` the host speaks, return
/// `Ok(())` iff they match, else a clear, actionable error BEFORE instantiation.
///
/// Wasmtime would itself reject a truly mismatched component at instantiate time,
/// but with a cryptic type-mismatch trap; this gives the friendly message and
/// explicitly catches the unversioned/legacy case (which can silently marshal
/// corrupted values because a rich-types bump shifts enum discriminants without
/// changing the import names).
///
/// - `package` — the contract package name, for the message (e.g.
///   `"duckdb:extension"` / `"sqlink:wasm"`).
/// - `ext_name` — the extension/component name, for the message.
pub fn check_component_contract(
    imported_major: Option<u64>,
    host_major: u64,
    package: &str,
    ext_name: &str,
) -> Result<()> {
    match imported_major {
        Some(major) if major == host_major => Ok(()),
        Some(major) => Err(anyhow::anyhow!(
            "extension '{ext_name}' targets {package} contract {major}.x but this host \
             speaks contract {host_major}.x; rebuild it against the current WIT (or use \
             the matching host version)"
        )),
        None => Err(anyhow::anyhow!(
            "extension '{ext_name}' targets an UNVERSIONED {package} contract (legacy, \
             pre-versioning) but this host speaks contract {host_major}.x; rebuild it \
             against the current WIT (or use the matching host version)"
        )),
    }
}

/// Minor-aware loader pre-check: given the `imported` `(major, minor)` a component
/// targets (as returned by [`component_contract_version`]) and the host's
/// `(host_major, host_minor)`, return `Ok(())` iff the component can load, else a
/// clear, actionable error BEFORE instantiation.
///
/// This is the full guard, subsuming [`check_component_contract`]:
///   - MAJOR mismatch + UNVERSIONED/legacy -> delegated to
///     [`check_component_contract`] (message + behavior preserved EXACTLY).
///   - SAME major but the component needs a HIGHER minor than the host provides
///     -> rejected: it imports interfaces (e.g. a later additive surface) the host
///     does not provide, so wasmtime would otherwise fail at instantiate with a
///     cryptic missing-import error. A LOWER or EQUAL minor is fine — MINORs are
///     additive, so a `MAJOR.k` component (`k <= host_minor`) imports a subset of
///     what the host provides (forward-compat: a 2.0 component on a 2.1 host loads).
///
/// - `package` — the contract package name, for the message.
/// - `ext_name` — the extension/component name, for the message.
pub fn check_component_version(
    imported: Option<(u64, u64)>,
    host_major: u64,
    host_minor: u64,
    package: &str,
    ext_name: &str,
) -> Result<()> {
    // MAJOR-mismatch + unversioned/legacy: reuse the existing guard so its message
    // and behavior are preserved byte-for-byte.
    check_component_contract(
        imported.map(|(major, _)| major),
        host_major,
        package,
        ext_name,
    )?;

    // MINOR gate: same major, but the component needs a HIGHER minor than this
    // host provides. (Lower/equal minor and the major/legacy cases handled above
    // are all Ok here.)
    if let Some((cmajor, cminor)) = imported {
        if cmajor == host_major && cminor > host_minor {
            return Err(anyhow::anyhow!(
                "extension '{ext_name}' needs {package} contract >= {cmajor}.{cminor} but this \
                 host speaks {host_major}.{host_minor}; upgrade the host (or use a build \
                 targeting {host_major}.{host_minor})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_major_is_ok() {
        assert!(check_component_contract(Some(2), 2, "duckdb:extension", "foo").is_ok());
        assert!(check_component_contract(Some(0), 0, "sqlink:wasm", "foo").is_ok());
    }

    #[test]
    fn mismatched_major_is_rejected_with_message() {
        let err = check_component_contract(Some(1), 2, "duckdb:extension", "foo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo"));
        assert!(err.contains("duckdb:extension contract 1.x"));
        assert!(err.contains("contract 2.x"));
    }

    #[test]
    fn unversioned_is_rejected_as_legacy() {
        let err = check_component_contract(None, 0, "sqlink:wasm", "bar")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bar"));
        assert!(err.contains("UNVERSIONED"));
        assert!(err.contains("sqlink:wasm"));
    }

    // ---- minor-aware guard (`check_component_version`) ----

    #[test]
    fn minor_equal_is_ok() {
        // A 2.1 component on a 2.1 host loads.
        assert!(check_component_version(Some((2, 1)), 2, 1, "duckdb:extension", "foo").is_ok());
    }

    #[test]
    fn minor_lower_is_ok() {
        // An existing 2.0 component on a 2.1 host loads (additive forward-compat:
        // it imports a subset of what the host provides).
        assert!(check_component_version(Some((2, 0)), 2, 1, "duckdb:extension", "foo").is_ok());
    }

    #[test]
    fn minor_higher_is_rejected_with_message() {
        // A 2.1 component on a 2.0 host is rejected BEFORE the cryptic
        // missing-import instantiate failure, with a friendly, actionable message.
        let err = check_component_version(Some((2, 1)), 2, 0, "duckdb:extension", "needs_copy")
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs_copy"));
        assert!(err.contains("needs duckdb:extension contract >= 2.1"));
        assert!(err.contains("speaks 2.0"));
    }

    #[test]
    fn minor_guard_preserves_major_and_legacy_behavior() {
        // Major mismatch -> the major-guard message is preserved exactly.
        let major = check_component_version(Some((1, 5)), 2, 1, "duckdb:extension", "foo")
            .unwrap_err()
            .to_string();
        assert!(major.contains("duckdb:extension contract 1.x"));
        assert!(major.contains("contract 2.x"));

        // Unversioned/legacy -> the legacy message is preserved exactly (the minor
        // gate is not its concern).
        let legacy = check_component_version(None, 2, 1, "sqlink:wasm", "bar")
            .unwrap_err()
            .to_string();
        assert!(legacy.contains("UNVERSIONED"));
        assert!(legacy.contains("sqlink:wasm"));
    }
}
