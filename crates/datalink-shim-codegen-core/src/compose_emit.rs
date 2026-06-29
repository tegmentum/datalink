//! Emit `compose.wac` for the wasm-component bridge crate.
//!
//! `wac plug` (0.10) only wires plug → socket. When the bridge's
//! composition needs plug → plug wiring — i.e. one plug provides
//! interfaces that another plug imports, or one plug's internal
//! implementation uses other interfaces from the same plug — `wac
//! plug` leaves those imports open, and the composed loadable
//! ends up with stray top-level imports the host can't satisfy.
//!
//! The W4a composition fix (#557fix / #563) introduced a per-bridge
//! `compose.wac` script that drives `wac compose` instead. The
//! script:
//!
//!   1. Instantiates each upstream package as a `let <alias> =
//!      new <pkg> { ... };` line, leaving its own wasi:* /
//!      contract:* imports open via the `...` ellipsis.
//!   2. Instantiates the stub-plug (when present) with its
//!      transitive imports wired from the matching upstream
//!      alias — `"mdb:types@0.1.0": mdb["mdb:types@0.1.0"]`.
//!   3. Instantiates the bridge, wiring each shim import slot to
//!      either the upstream alias (when upstream exports it) or
//!      the stub alias (W4a additions). The trailing `...` lets
//!      contract:* and wasi:* fall through as top-level
//!      composition imports.
//!   4. `export bridge...;` re-exports the bridge's exported
//!      contract surface as the composition's surface.
//!
//! Emission decision (`should_emit_compose_wac`):
//!
//!   * **Stub-plug present** — always emit. The stub-plug is the
//!     direct signal that plug → plug wiring is needed.
//!   * **Single shim namespace** (e.g. mobilitydb:temporal alone) —
//!     emit, because the upstream component implements its
//!     interfaces in terms of other interfaces from the SAME
//!     package and `wac plug` won't synthesise that self-wiring.
//!   * **Multiple shim namespaces** (e.g. postgis:wasm +
//!     sfcgal:component) — SKIP. The upstream side is a
//!     pre-composed `<primary>-composed.wasm` that packs all
//!     namespaces and resolves their internal interface usage
//!     internally, so plain `wac plug` against that wasm
//!     satisfies the bridge cleanly. The postgis bridge fits
//!     this case today.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::wit_parse::WitPackage;

/// One `import <ns>:<name>/<iface>@<ver>;` line teased apart into
/// its constituent parts so it can be re-emitted as a wac wiring
/// `"<ns>:<name>/<iface>@<ver>": <alias>["<ns>:<name>/<iface>@<ver>"]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportRef {
    /// Package namespace + name, e.g. `mobilitydb:temporal`.
    pkg: String,
    /// Interface, e.g. `tbool-ops`.
    iface: String,
    /// Version, e.g. `0.1.0`.
    version: String,
}

impl ImportRef {
    fn full(&self) -> String {
        format!("{}/{}@{}", self.pkg, self.iface, self.version)
    }
}

/// Stub-plug summary parsed from `<out_dir>/stub-plug/wit/world.wit`.
#[derive(Debug)]
struct StubPlug {
    /// Stub-plug package + version, e.g.
    /// `sqlink-bridge:mobilitydb-w4a-stub@0.1.0`.
    pkg: String,
    version: String,
    /// Interfaces the stub-plug imports transitively from
    /// upstream. These need explicit plug → plug wiring inside
    /// the stub instantiation block.
    imports: Vec<ImportRef>,
    /// Interfaces the stub-plug exports (the W4a additions).
    /// Used to partition the bridge's import list between
    /// "from upstream" and "from stub".
    exports: Vec<ImportRef>,
}

/// Entry point invoked from `wasm_target::emit`. Writes
/// `<out_dir>/compose.wac` when the bridge needs explicit
/// `wac compose` wiring (see `should_emit_compose_wac` for the
/// rule), otherwise returns `Ok(false)` (skipped).
///
/// * `primary` — primary extension name (e.g. `mobilitydb`).
/// * `bridge_pkg_name` — full WIT package name of the bridge
///   crate, NOT including `@<version>`. Per-target prefix:
///   sqlite-emit passes `"sqlink-bridge:<primary>"`,
///   datafission-emit passes `"datafission-bridge:<primary>"`.
/// * `shim_packages` — the same set `emit_wit::write_world`
///   enumerates: the bridge's full upstream import surface.
pub fn write_compose_wac(
    out_dir: &Path,
    primary: &str,
    bridge_pkg_name: &str,
    shim_packages: &[WitPackage],
) -> Result<bool> {
    let stub_world = out_dir.join("stub-plug/wit/world.wit");
    let stub = if stub_world.is_file() {
        let text = fs::read_to_string(&stub_world)
            .with_context(|| format!("reading {}", stub_world.display()))?;
        Some(parse_stub_world(&text).with_context(|| {
            format!("parsing stub-plug world at {}", stub_world.display())
        })?)
    } else {
        None
    };
    if !should_emit_compose_wac(stub.is_some(), shim_packages) {
        return Ok(false);
    }
    let bridge_imports = collect_bridge_imports(shim_packages);
    let body = render_compose(primary, bridge_pkg_name, stub.as_ref(), &bridge_imports);
    let dest = out_dir.join("compose.wac");
    fs::write(&dest, body)
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(true)
}

/// Decision rule (per module doc-comment).
///
/// Returns `true` iff either:
///   * a stub-plug is present (always needs plug→plug wiring), OR
///   * the bridge imports from exactly ONE shim namespace (the
///     upstream component lacks a pre-composed wrapper, so `wac
///     plug` can't satisfy its own internal cross-interface usage).
fn should_emit_compose_wac(has_stub: bool, shim_packages: &[WitPackage]) -> bool {
    if has_stub {
        return true;
    }
    let unique_namespaces: BTreeSet<&str> = shim_packages
        .iter()
        .map(|p| p.ns_name.split(':').next().unwrap_or(""))
        .filter(|s| !s.is_empty())
        .collect();
    unique_namespaces.len() == 1
}

/// Enumerate every `<pkg>/<iface>@<ver>` the bridge's world.wit
/// imports. Mirrors `emit_wit::render_world`'s loop over
/// `shim_packages`: every interface in every parsed shim package
/// becomes one import line.
fn collect_bridge_imports(shim_packages: &[WitPackage]) -> Vec<ImportRef> {
    let mut out = Vec::new();
    for pkg in shim_packages {
        for iface in &pkg.interfaces {
            out.push(ImportRef {
                pkg: pkg.ns_name.clone(),
                iface: iface.clone(),
                version: pkg.version.clone(),
            });
        }
    }
    out
}

/// Parse the stub-plug `world.wit`. Extracts the `package <ns>:<name>@<ver>;`
/// declaration plus every `import <ns>:<name>/<iface>@<ver>;` and
/// `export <ns>:<name>/<iface>@<ver>;` line inside the world block.
fn parse_stub_world(text: &str) -> Result<StubPlug> {
    let mut pkg = None::<String>;
    let mut version = None::<String>;
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package ") {
            // `ns:name@version;` — possibly trailing semicolon.
            let rest = rest.trim().trim_end_matches(';').trim();
            if let Some((ns_name, ver)) = rest.split_once('@') {
                pkg = Some(ns_name.trim().to_string());
                version = Some(ver.trim().to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            if let Some(r) = parse_import_ref(rest) {
                imports.push(r);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some(r) = parse_import_ref(rest) {
                exports.push(r);
            }
            continue;
        }
    }
    let pkg = pkg.ok_or_else(|| {
        anyhow::anyhow!("stub-plug world.wit has no `package <ns>:<name>@<ver>;` line")
    })?;
    let version = version.unwrap_or_else(|| "0.1.0".to_string());
    Ok(StubPlug {
        pkg,
        version,
        imports,
        exports,
    })
}

/// Parse `<ns>:<name>/<iface>@<ver>;` (or `... ;` or `... // ...`).
fn parse_import_ref(s: &str) -> Option<ImportRef> {
    let s = s.trim().trim_end_matches(';').trim();
    let (pkg, iface_ver) = s.split_once('/')?;
    let (iface, version) = iface_ver.split_once('@')?;
    Some(ImportRef {
        pkg: pkg.trim().to_string(),
        iface: iface.trim().to_string(),
        version: version.trim().to_string(),
    })
}

/// Strip a single trailing `// ...` line comment. Block comments
/// (`/* ... */`) aren't expected in world.wit body lines.
fn strip_comment(line: &str) -> &str {
    if let Some(idx) = line.find("//") {
        &line[..idx]
    } else {
        line
    }
}

fn render_compose(
    primary: &str,
    bridge_pkg_name: &str,
    stub: Option<&StubPlug>,
    bridge_imports: &[ImportRef],
) -> String {
    // wac syntax: `let <alias> = new <pkg> { ... };` does NOT
    // accept `@<version>` on the package ref. Version pins go
    // on the interface refs inside the wiring block.
    //
    // The composed package name reuses the bridge's prefix
    // (`sqlink-bridge:` / `datafission-bridge:`) so a single
    // wac workspace can host both targets without colliding.
    let composed_pkg = format!("{bridge_pkg_name}-composed");

    // Collect every distinct upstream package the bridge or
    // stub-plug needs wired. Each becomes one `let <alias> = new
    // <pkg> { ... };` line.
    let mut upstream_pkgs: BTreeSet<String> = BTreeSet::new();
    for imp in bridge_imports {
        // Anything in stub-exports comes from `stub`, not from
        // upstream. We still need the upstream package alias for
        // every OTHER import from the same package.
        upstream_pkgs.insert(imp.pkg.clone());
    }
    if let Some(stub) = stub {
        for imp in &stub.imports {
            upstream_pkgs.insert(imp.pkg.clone());
        }
    }

    // Aliases: deterministic per-namespace short labels so the
    // emitted script reads close to the hand-written one
    // (`mdb`, `pg`, ...). Fall back to the namespace itself for
    // unknown shims.
    let aliases: BTreeMap<String, String> = upstream_pkgs
        .iter()
        .map(|p| (p.clone(), package_alias(p)))
        .collect();

    let stub_exports: BTreeSet<String> = stub
        .map(|s| s.exports.iter().map(|i| i.full()).collect())
        .unwrap_or_default();

    let mut s = String::new();
    write_header(&mut s, primary, bridge_pkg_name, stub, &aliases);
    s.push_str(&format!("package {composed_pkg}@{ver};\n\n", ver = "0.1.0"));

    // Upstream instantiations.
    s.push_str("// Upstream shim instantiations. The `...` ellipsis lets each\n");
    s.push_str("// instance's wasi:* (and any other unmentioned) imports stay\n");
    s.push_str("// open so the host satisfies them at load time.\n");
    for pkg in &upstream_pkgs {
        let alias = &aliases[pkg];
        s.push_str(&format!("let {alias} = new {pkg} {{ ... }};\n"));
    }
    s.push('\n');

    // Stub-plug instantiation — explicit plug → plug wiring of
    // its transitive imports from upstream aliases. Skipped when
    // no stub-plug is present (single-namespace path).
    if let Some(stub) = stub {
        s.push_str("// Stub-plug provides the bridge's W4a-vendored interfaces with\n");
        s.push_str("// empty-row bodies. Its transitive imports (`types`, the ops\n");
        s.push_str("// interfaces whose types the W4a additions reference) get wired\n");
        s.push_str("// from the matching upstream package here — `wac plug` cannot\n");
        s.push_str("// synthesise this plug → plug wiring on its own.\n");
        let _ = &stub.version; // version pin is implicit; wac `new` rejects @ver
        s.push_str(&format!(
            "let stub = new {stub_pkg} {{\n",
            stub_pkg = stub.pkg,
        ));
        let mut sorted_imports = stub.imports.clone();
        sorted_imports.sort();
        for imp in &sorted_imports {
            let alias = aliases
                .get(&imp.pkg)
                .cloned()
                .unwrap_or_else(|| package_alias(&imp.pkg));
            let full = imp.full();
            s.push_str(&format!("    \"{full}\": {alias}[\"{full}\"],\n"));
        }
        s.push_str("    ...\n");
        s.push_str("};\n\n");
    }

    // Bridge instantiation — partition every shim import between
    // upstream alias (default) and stub alias (W4a additions).
    s.push_str("// Bridge instantiation. Each shim import is wired to either the\n");
    s.push_str("// upstream alias (default) or the stub alias (W4a additions).\n");
    s.push_str("// The trailing `...` lets contract:* and wasi:* remain open as\n");
    s.push_str("// top-level composition imports for the host to satisfy.\n");
    s.push_str(&format!("let bridge = new {bridge_pkg_name} {{\n"));
    let mut sorted_bridge_imports = bridge_imports.to_vec();
    sorted_bridge_imports.sort();
    for imp in &sorted_bridge_imports {
        let full = imp.full();
        let source = if stub_exports.contains(&full) {
            "stub".to_string()
        } else {
            aliases
                .get(&imp.pkg)
                .cloned()
                .unwrap_or_else(|| package_alias(&imp.pkg))
        };
        s.push_str(&format!("    \"{full}\": {source}[\"{full}\"],\n"));
    }
    s.push_str("    ...\n");
    s.push_str("};\n\n");

    s.push_str("// Re-export the bridge's contract surface as the composition's\n");
    s.push_str("// surface so the composed loadable presents the canonical\n");
    s.push_str("// host contract (sqlite:extension / datafission:extension / ...).\n");
    s.push_str("export bridge...;\n");

    s
}

fn write_header(
    s: &mut String,
    primary: &str,
    bridge_pkg_name: &str,
    stub: Option<&StubPlug>,
    aliases: &BTreeMap<String, String>,
) {
    s.push_str(&format!(
        "// Auto-generated by sqlink-shim-codegen — compose.wac for\n\
        // {primary} bridge ({bridge_pkg_name}).\n\
        //\n\
        // wac plug (0.10) wires plug → socket only. This bridge\n\
        // needs `wac compose` because either a W4a stub-plug supplies\n\
        // interfaces upstream exports, or the upstream component's\n\
        // own interfaces reference other interfaces from the same\n\
        // package (plug → plug wiring `wac plug` cannot synthesise).\n\
        //\n\
        // Aliases used:\n"
    ));
    for (pkg, alias) in aliases {
        s.push_str(&format!("//   {alias:>8} = {pkg}\n"));
    }
    if let Some(stub) = stub {
        s.push_str(&format!(
            "//   {alias:>8} = {pkg}\n",
            alias = "stub",
            pkg = stub.pkg,
        ));
    }
    s.push_str(&format!(
        "//   {alias:>8} = {pkg}\n",
        alias = "bridge",
        pkg = bridge_pkg_name,
    ));
    s.push_str(
        "//\n\
        // Run with:\n\
        //   wac compose <path-to-this-script> \\\n\
        //     -d <pkg>=<path-to-wasm> \\\n\
        //     ... \\\n\
        //     -o <out>.wasm\n\
        //\n",
    );
}

/// Pick a short, readable alias for a package namespace. The
/// hand-written compose.wac uses `mdb` for mobilitydb and `pg`
/// for postgis; we mirror those so the auto-emitted script reads
/// the same. Unknown namespaces fall back to the first segment of
/// the package name.
fn package_alias(pkg: &str) -> String {
    let ns = pkg.split(':').next().unwrap_or(pkg);
    match ns {
        "mobilitydb" => "mdb".to_string(),
        "postgis" => "pg".to_string(),
        other => other.replace('-', "_"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stub_world_basics() {
        let txt = r#"
package sqlink-bridge:mobilitydb-w4a-stub@0.1.0;

world w4a-stub {
    import mobilitydb:temporal/types@0.1.0;
    import mobilitydb:temporal/tbool-ops@0.1.0;

    export mobilitydb:temporal/typed-join-ops@0.1.0;
    export mobilitydb:temporal/time-split-ops@0.1.0;
}
"#;
        let stub = parse_stub_world(txt).unwrap();
        assert_eq!(stub.pkg, "sqlink-bridge:mobilitydb-w4a-stub");
        assert_eq!(stub.version, "0.1.0");
        assert_eq!(stub.imports.len(), 2);
        assert_eq!(stub.exports.len(), 2);
        assert_eq!(stub.imports[0].iface, "types");
        assert_eq!(stub.exports[1].iface, "time-split-ops");
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let txt = r#"
// header comment
package foo:bar@1.2.3;
// world doc
world w {
    // doc
    import a:b/c@1.0.0; // trailing
    export a:b/d@1.0.0;
}
"#;
        let stub = parse_stub_world(txt).unwrap();
        assert_eq!(stub.pkg, "foo:bar");
        assert_eq!(stub.version, "1.2.3");
        assert_eq!(stub.imports.len(), 1);
        assert_eq!(stub.exports.len(), 1);
    }

    #[test]
    fn alias_table_known_namespaces() {
        assert_eq!(package_alias("mobilitydb:temporal"), "mdb");
        assert_eq!(package_alias("postgis:wasm"), "pg");
        assert_eq!(package_alias("postgis:composed"), "pg");
        assert_eq!(package_alias("foo:bar"), "foo");
    }

    #[test]
    fn renders_compose_with_partitioned_imports() {
        let stub = StubPlug {
            pkg: "sqlink-bridge:mobilitydb-w4a-stub".to_string(),
            version: "0.1.0".to_string(),
            imports: vec![ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "types".to_string(),
                version: "0.1.0".to_string(),
            }],
            exports: vec![ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "typed-join-ops".to_string(),
                version: "0.1.0".to_string(),
            }],
        };
        let bridge_imports = vec![
            ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "types".to_string(),
                version: "0.1.0".to_string(),
            },
            ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "typed-join-ops".to_string(),
                version: "0.1.0".to_string(),
            },
        ];
        let out = render_compose(
            "mobilitydb",
            "sqlink-bridge:mobilitydb",
            Some(&stub),
            &bridge_imports,
        );
        assert!(out.contains("let mdb = new mobilitydb:temporal { ... };"));
        assert!(out.contains("let stub = new sqlink-bridge:mobilitydb-w4a-stub {"));
        assert!(out.contains("let bridge = new sqlink-bridge:mobilitydb {"));
        assert!(out.contains(
            "\"mobilitydb:temporal/types@0.1.0\": mdb[\"mobilitydb:temporal/types@0.1.0\"]"
        ));
        // W4a addition routes from stub.
        assert!(out.contains(
            "\"mobilitydb:temporal/typed-join-ops@0.1.0\": stub[\"mobilitydb:temporal/typed-join-ops@0.1.0\"]"
        ));
        assert!(out.contains("export bridge...;"));
    }

    #[test]
    fn renders_compose_without_stub_for_datafission() {
        // mobilitydb-datafission case: single shim namespace, no
        // stub-plug. wac plug fails because mdb-temporal-wasm's own
        // interfaces reference other interfaces from the same
        // package; wac compose with explicit `new mobilitydb:temporal
        // { ... }` resolves them through a single component
        // instance.
        let bridge_imports = vec![
            ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "types".to_string(),
                version: "0.1.0".to_string(),
            },
            ImportRef {
                pkg: "mobilitydb:temporal".to_string(),
                iface: "tint-ops".to_string(),
                version: "0.1.0".to_string(),
            },
        ];
        let out = render_compose(
            "mobilitydb",
            "datafission-bridge:mobilitydb",
            None,
            &bridge_imports,
        );
        assert!(out.contains("let mdb = new mobilitydb:temporal { ... };"));
        assert!(out.contains("let bridge = new datafission-bridge:mobilitydb {"));
        assert!(out.contains(
            "\"mobilitydb:temporal/tint-ops@0.1.0\": mdb[\"mobilitydb:temporal/tint-ops@0.1.0\"]"
        ));
        // No stub instantiation expected when stub is None.
        assert!(!out.contains("let stub = new"));
        // Composed package mirrors the bridge prefix.
        assert!(out.contains("package datafission-bridge:mobilitydb-composed@0.1.0;"));
        assert!(out.contains("export bridge...;"));
    }

    #[test]
    fn should_emit_rule_single_namespace_emits() {
        let pkgs = vec![WitPackage {
            ns_name: "mobilitydb:temporal".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["tint-ops".to_string()],
            records: vec![],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        }];
        assert!(super::should_emit_compose_wac(false, &pkgs));
    }

    #[test]
    fn should_emit_rule_multi_namespace_skips() {
        let pkgs = vec![
            WitPackage {
                ns_name: "postgis:wasm".to_string(),
                version: "0.1.0".to_string(),
                interfaces: vec!["postgis-accessors".to_string()],
                records: vec![],
                resources: vec![],
                variants: vec![],
                enums: vec![],
                flags: vec![],
                type_aliases: vec![],
            },
            WitPackage {
                ns_name: "sfcgal:component".to_string(),
                version: "0.1.0".to_string(),
                interfaces: vec!["sfcgal-ops".to_string()],
                records: vec![],
                resources: vec![],
                variants: vec![],
                enums: vec![],
                flags: vec![],
                type_aliases: vec![],
            },
        ];
        assert!(!super::should_emit_compose_wac(false, &pkgs));
        // Stub-plug override forces emit even with multi-namespace.
        assert!(super::should_emit_compose_wac(true, &pkgs));
    }
}
