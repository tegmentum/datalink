//! Emit the WIT world + vendored deps for the wasm-component
//! bridge.
//!
//! The world imports the upstream shim's interfaces and exports
//! the canonical `sqlite:extension/*` surface. The vendored
//! `deps/` directory holds the dependency WIT packages that
//! `wit-bindgen::generate!` resolves at build time.
//!
//! ## Per-shim source layout
//!
//! `source_shim_deps_dir(primary)` is re-exported from
//! `datalink-shim-codegen-core::wit_paths` (#654 lift). See that
//! module's docs for the four-step resolution order — every emit
//! target now flows through the same upstream-synthesis path so
//! a new function added upstream wires through every target
//! without per-target env overrides.
//!
//! ## Phase D: dynamic world.wit
//!
//! Rather than a hardcoded constant, `write_world` now inspects
//! the discovered deps tree, enumerates each package and its
//! interfaces, and emits one `import <ns>:<name>/<iface>@<ver>;`
//! line per interface. The `sqlite:extension/*` imports + exports
//! stay fixed at the contract level — the host bindgen targets a
//! specific surface there and the codegen ferries through it
//! verbatim. Helper-component packages (sfcgal-component for
//! postgis) are imported alongside the primary shim package via
//! the same enumeration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use shim_bridge_codegen_core::BridgePlan;
use datalink_shim_codegen_core::kebab_fix::kebab_fix_wit;
use datalink_shim_codegen_core::record_registry::RecordType;
use datalink_shim_codegen_core::wit_parse::{self, WitPackage};

// #654: WIT-deps resolution (incl. #651 upstream synthesis) lifted
// to `datalink-shim-codegen-core::wit_paths` so every emit target
// shares one implementation. Re-exported so existing callers like
// `emit_wit::source_shim_deps_dir(...)` keep working unchanged.
pub use datalink_shim_codegen_core::wit_paths::source_shim_deps_dir;

/// Write `wit/world.wit` at the given path. Builds the world body
/// dynamically by inspecting the source-shim deps tree.
pub fn write_world(plan: &BridgePlan, dest: &Path) -> Result<()> {
    let primary = primary_extension_name(plan);
    let shim_deps = source_shim_deps_dir(primary)?;
    let shim_packages = discover_shim_packages(&shim_deps)?;
    let contract_pkg = discover_contract_package()?;
    // Phase C: serde-ops records are restricted to the PRIMARY shim
    // package only. Helper-component records (sfcgal-component types
    // for postgis, proj/dbscan/etc. for mobilitydb) aren't the
    // bridge's responsibility — they live in their own composed
    // packages with their own serde paths.
    let records = datalink_shim_codegen_core::record_registry::build(&shim_packages, primary)
        .into_iter()
        .filter(|r| package_belongs_to_primary(&r.package, primary))
        .collect::<Vec<_>>();
    // #794: skip records whose fields (transitively) reference a
    // resource-kind upstream identifier. The bridge can't emit a
    // ciborium codec for such a record — wit-bindgen suppresses
    // Serialize/Deserialize on the resource-typed handle, so the
    // record can't derive them either. `filter_serde_ops_records`
    // walks the type source map + primary alias map so post-#785 the
    // filter matches the same set the emit_lib codec walker uses.
    let records = filter_serde_ops_records(&records, &shim_packages, primary);
    let world = render_world(primary, &shim_packages, &contract_pkg, &records);
    fs::write(dest, world).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// #794: shared filter — remove records that can't participate in
/// the serde-ops surface because they reference a resource-kind
/// identifier in one of their field types. The bridge's wit-value
/// codec (ciborium round-trip) requires Serialize + Deserialize on
/// both the LOCAL serde-ops copy and the UPSTREAM binding; a resource
/// field breaks both derives. The dispatch layer keeps using the
/// upstream binding directly (no wit-value codec) so upstream
/// functions returning such records still wire.
pub fn filter_serde_ops_records(
    records: &[RecordType],
    shim_packages: &[wit_parse::WitPackage],
    primary: &str,
) -> Vec<RecordType> {
    let type_source_map = build_type_source_map(shim_packages);
    let alias_map: std::collections::BTreeMap<String, String> = shim_packages
        .iter()
        .filter(|p| package_belongs_to_primary(&p.ns_name, primary))
        .flat_map(|p| p.type_aliases.iter())
        .map(|a| (a.kebab_name.clone(), a.body.clone()))
        .collect();
    records
        .iter()
        .filter(|r| !record_has_resource_field(r, &alias_map, &type_source_map))
        .cloned()
        .collect()
}

/// #794: true when any of the record's field types (after alias
/// resolution) references an identifier whose `TypeKind` is
/// `Resource`. Resources in wit-bindgen materialise as opaque
/// handles without Serialize / Deserialize impls, so a record with
/// a resource field can't take part in the ciborium codec surface.
fn record_has_resource_field(
    record: &RecordType,
    alias_map: &std::collections::BTreeMap<String, String>,
    type_source_map: &std::collections::BTreeMap<String, TypeSource>,
) -> bool {
    for (_fname, ftype) in &record.fields {
        let resolved = inline_aliases(ftype, alias_map);
        let mut idents: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let local_record_names: std::collections::BTreeSet<&str> =
            std::collections::BTreeSet::new();
        collect_type_idents(&resolved, &local_record_names, &mut idents);
        for ident in &idents {
            if let Some(src) = type_source_map.get(ident) {
                if matches!(src.kind, TypeKind::Resource) {
                    return true;
                }
            }
        }
    }
    false
}

/// Returns true when `package` is the primary shim's own package
/// (e.g. `postgis:wasm` is owned by primary `postgis`,
/// `mobilitydb:temporal` by `mobilitydb`). Helper-component
/// packages return false so their records skip serde-ops emit.
pub fn package_belongs_to_primary(package: &str, primary: &str) -> bool {
    package.split(':').next().map(|ns| ns == primary).unwrap_or(false)
}

/// Copy the dependency WIT tree into `wit/deps/`.
///
/// Every subdir of the source shim deps tree that holds a
/// well-formed package is copied as-is. The contract package
/// (`sqlite-extension/`) is always sourced from the host-loader
/// `wit/` directly so the generated bridge picks up host-bindgen
/// invariants without needing the shim-bridge tree to mirror them.
pub fn write_deps(plan: &BridgePlan, deps_dir: &Path) -> Result<()> {
    let primary = primary_extension_name(plan);
    let shim_src = source_shim_deps_dir(primary)?;
    for entry in fs::read_dir(&shim_src)? {
        let entry = entry?;
        let from = entry.path();
        if !from.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == "sqlite-extension" {
            // Sourced from the host wit dir below.
            continue;
        }
        let to = deps_dir.join(&name);
        copy_tree(&from, &to)
            .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    }
    let host_wit = source_host_wit_dir()?;
    let host_dst = deps_dir.join("sqlite-extension");
    copy_tree(&host_wit, &host_dst).with_context(|| {
        format!(
            "copying host wit/ {} -> {}",
            host_wit.display(),
            host_dst.display()
        )
    })?;
    Ok(())
}

/// Locate sqlink's `sqlite-loader-wit/wit/` directory — the
/// canonical sqlite:extension WIT source the host bindgen targets.
///
/// Resolution order:
///   1. `$SQLINK_LOADER_WIT` (explicit override)
///   2. `$HOME/git/sqlink/sqlite-loader-wit/wit`
///   3. `../sqlink/sqlite-loader-wit/wit`
fn source_host_wit_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SQLINK_LOADER_WIT") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(anyhow!(
            "SQLINK_LOADER_WIT={} does not exist",
            p.display()
        ));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("git/sqlink/sqlite-loader-wit/wit");
        if p.is_dir() {
            return Ok(p);
        }
    }
    let rel = PathBuf::from("../sqlink/sqlite-loader-wit/wit");
    if rel.is_dir() {
        return Ok(rel);
    }
    Err(anyhow!(
        "cannot locate sqlite-loader-wit/wit. Set \
         SQLINK_LOADER_WIT=/path/to/sqlink/sqlite-loader-wit/wit"
    ))
}

/// Walk the shim deps tree and parse every package subdir into a
/// `WitPackage`. The contract package (`sqlite-extension`) is
/// EXCLUDED — it's loaded separately from the host wit dir.
pub fn discover_shim_packages(deps_root: &Path) -> Result<Vec<WitPackage>> {
    let mut out = Vec::new();
    if !deps_root.is_dir() {
        return Ok(out);
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(deps_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for d in entries {
        let name = d.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "sqlite-extension" {
            continue;
        }
        if let Some(pkg) = wit_parse::parse_package_dir(&d)
            .with_context(|| format!("parsing {}", d.display()))?
        {
            out.push(pkg);
        }
    }
    Ok(out)
}

/// Parse the host wit dir's contract package (`sqlite:extension`).
pub fn discover_contract_package() -> Result<WitPackage> {
    let host = source_host_wit_dir()?;
    let pkg = wit_parse::parse_package_dir(&host)?.ok_or_else(|| {
        anyhow!(
            "host wit dir {} has no parseable package declaration",
            host.display()
        )
    })?;
    Ok(pkg)
}

/// Render `world.wit` from the parsed packages. Imports each
/// interface declared by every shim package; imports the fixed
/// contract surface; exports the fixed contract surface. Phase C
/// also emits a `serde-ops` interface in the bridge's own package
/// declaring per-record `<record>-from-canon-cbor` +
/// `<record>-to-canon-cbor` exports for every record discovered
/// in the shim WIT packages.
pub fn render_world(
    primary: &str,
    shim_packages: &[WitPackage],
    contract_pkg: &WitPackage,
    records: &[RecordType],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "package sqlink-bridge:{primary}@0.1.0;\n\n",
    ));

    // Phase E: the per-record `<record>-from-canon-cbor` +
    // `<record>-to-canon-cbor` surface is declared as a real
    // `interface serde-ops { ... }` block at the package level and
    // exported by the world below. Records are defined LOCALLY
    // (not `use`'d from upstream) so the resulting composed
    // component doesn't need to re-export the upstream interfaces —
    // a WASM component-model invariant requires exported instances
    // to reference only also-exported types, and the upstream
    // shim interfaces become non-exported once wac plug satisfies
    // them as imports.
    //
    // Type aliases (`type X = Y;`) declared in the primary shim are
    // inline-substituted into each record's field types so the local
    // serde-ops doesn't need to also redeclare the alias.
    let alias_map: std::collections::BTreeMap<String, String> = shim_packages
        .iter()
        .filter(|p| package_belongs_to_primary(&p.ns_name, primary))
        .flat_map(|p| p.type_aliases.iter())
        .map(|a| (a.kebab_name.clone(), a.body.clone()))
        .collect();
    // #785: build a name → (ns, version, interface) map covering
    // every cross-interface type identifier a record field might
    // reference (resources, variants, flags, records, enums). Records
    // are duplicated locally into serde-ops; identifiers that miss
    // both the primary-package clone path AND the alias-inline path
    // (canonical example: `geometry` in `pixel-vec-entry.geom` — a
    // resource in `postgis-types`, not inlineable) trigger a `use`
    // directive at the top of the interface so wit-bindgen resolves
    // the reference.
    let type_source_map = build_type_source_map(shim_packages);
    let serde_ops_block =
        render_serde_ops_interface(records, &alias_map, &type_source_map, primary);
    if !serde_ops_block.is_empty() {
        // Splice the local-enum block into the placeholder. The
        // enums we duplicate are those referenced by any record's
        // field types. Source bodies come from `shim_packages`'s
        // parsed enums.
        let referenced =
            collect_referenced_enum_names(records);
        let mut enums_block = String::new();
        if !referenced.is_empty() {
            for pkg in shim_packages {
                if !package_belongs_to_primary(&pkg.ns_name, primary) {
                    continue;
                }
                for e in &pkg.enums {
                    if referenced.contains(&e.kebab_name) {
                        enums_block
                            .push_str(&render_local_enum(&e.kebab_name, &e.cases));
                    }
                }
            }
        }
        let spliced =
            serde_ops_block.replace("__SERDE_OPS_LOCAL_ENUMS_PLACEHOLDER__\n", &enums_block);
        s.push_str(&spliced);
        s.push('\n');
    }

    s.push_str("/// Generated by sqlink-shim-codegen (target=wasm-component).\n");
    s.push_str("/// Bridges the shim's WIT-exposed surface onto the canonical\n");
    s.push_str("/// `sqlite:extension/*` contract. Import list is derived from\n");
    s.push_str("/// the shim's vendored WIT packages; export list is the fixed\n");
    s.push_str("/// contract export quartet (metadata + scalar + aggregate + vtab).\n");
    s.push_str("world bridge {\n");

    // Shim imports — every interface in every shim package.
    for pkg in shim_packages {
        for iface in &pkg.interfaces {
            s.push_str(&format!(
                "    import {ns}/{iface}@{ver};\n",
                ns = pkg.ns_name,
                iface = iface,
                ver = pkg.version,
            ));
        }
    }
    s.push('\n');

    // Contract imports + exports — derived from the host wit dir's
    // sqlite:extension package version, with the fixed
    // import/export interface lists baked in. The fixed lists are
    // the surface the host bindgen targets and must match
    // sqlite-loader-wit/wit/.
    let contract_ns = &contract_pkg.ns_name; // "sqlite:extension"
    let contract_ver = &contract_pkg.version;
    for iface in CONTRACT_IMPORTS {
        s.push_str(&format!(
            "    import {contract_ns}/{iface}@{contract_ver};\n"
        ));
    }
    s.push('\n');
    for iface in CONTRACT_EXPORTS {
        s.push_str(&format!(
            "    export {contract_ns}/{iface}@{contract_ver};\n"
        ));
    }
    // Phase E: export the bridge's own serde-ops interface so the
    // typed-value-binding decoder/encoder imports the host sees in
    // the manifest resolve to real wasm-side symbols. The interface
    // body was rendered at the top of the file (above the world
    // block); here we just add `export serde-ops;` so the world
    // surfaces it to consumers. Skipped when the registry has no
    // records — no records means no serde-ops block was emitted.
    if !records.is_empty() {
        s.push_str("\n    export serde-ops;\n");
    }
    s.push_str("}\n");
    s
}

/// Render the `interface serde-ops { ... }` block declaring per-record
/// `<record>-from-canon-cbor` + `<record>-to-canon-cbor` functions.
///
/// Phase E: records (and their referenced enums) are declared
/// LOCALLY inside the serde-ops interface rather than `use`'d from
/// the upstream shim packages. The reason is a WASM component-model
/// invariant: a component cannot export an instance whose interface
/// references types not also exported by the component. After wac
/// plug satisfies the upstream `postgis:wasm/*` imports, those
/// interfaces are no longer exported by the resulting component, so
/// re-exporting `serde-ops { use postgis:wasm/postgis-types.{coord};
/// ... }` triggers a component-model validation error.
///
/// The local copies mirror the upstream shape verbatim, so the
/// codec's wire format matches between the bridge and any other
/// consumer that runs canon-cbor over the upstream record shape.
/// The bridge's dispatch arms convert LOCAL → UPSTREAM
/// (field-by-field; same shape) before calling upstream functions.
///
/// Returns an empty string when the registry has no records (no
/// serde-ops block is emitted in that case).
fn render_serde_ops_interface(
    records: &[RecordType],
    alias_map: &std::collections::BTreeMap<String, String>,
    type_source_map: &std::collections::BTreeMap<String, TypeSource>,
    primary: &str,
) -> String {
    if records.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str("/// Phase E wit-value codec surface: per-record\n");
    s.push_str("/// canonical-CBOR encoder + decoder.\n");
    s.push_str("///\n");
    s.push_str("/// Records are declared LOCALLY here (rather than\n");
    s.push_str("/// `use`'d from upstream shim packages) so the\n");
    s.push_str("/// composed component can export serde-ops without\n");
    s.push_str("/// also re-exporting the upstream interfaces — the\n");
    s.push_str("/// component-model export invariant requires any\n");
    s.push_str("/// type referenced by an exported instance to itself\n");
    s.push_str("/// be exported. Bodies use ciborium against the\n");
    s.push_str("/// wit-bindgen-generated local types (which derive\n");
    s.push_str("/// Serialize+Deserialize via additional_derives).\n");
    s.push_str("///\n");
    s.push_str("/// #785: identifiers referenced by a record field that\n");
    s.push_str("/// resolve to a non-cloneable upstream type (resources\n");
    s.push_str("/// like `geometry`; variants; flags) trigger a `use`\n");
    s.push_str("/// directive at the top of this interface. Records\n");
    s.push_str("/// still declared locally; the `use` covers only the\n");
    s.push_str("/// referenced-but-uncloneable subset.\n");
    s.push_str("interface serde-ops {\n");

    // Collect referenced enums (transitive closure over each
    // record's field types). The wit_parse-generated enum cases
    // give us the body to copy locally.
    // NOTE: only same-package enums are emitted; an upstream record
    // that references an enum from a helper package would need that
    // helper's enum duplicated too — Phase E+ deals with that if
    // it arises (today's primary records reference only
    // primary-package enums).
    let referenced_enum_names: std::collections::BTreeSet<String> =
        collect_referenced_enum_names(records);
    if !referenced_enum_names.is_empty() {
        // We don't have direct access to the parsed enums here.
        // The caller (`render_world`) walked `shim_packages` and
        // could pass the enum bodies, but to keep render_world's
        // signature stable we stub a placeholder marker the caller
        // will substitute. Emit a sentinel comment so the codegen
        // surfaces a maintainer note if any referenced-but-unknown
        // enum lands here in the future.
        // For Phase E the actual enum body emission happens in
        // `render_world` via `render_local_enums_block`.
    }

    // #785: emit `use <pkg>/<iface>@<ver>.{<type>};` directives for
    // every cross-interface identifier referenced by a record field
    // that ISN'T handled by the local-clone paths (records duplicated
    // below, primary-package enums cloned via the placeholder,
    // primary-package aliases inlined by `inline_aliases`). The
    // canonical trigger is `pixel-vec-entry.geom: geometry` — a
    // resource in `postgis-types`, uncloneable, so we `use` it. Same
    // treatment covers variants/flags and any cross-package record
    // whose owning package isn't inlined into serde-ops.
    let use_block = render_use_directives(
        records,
        alias_map,
        type_source_map,
        primary,
    );
    s.push_str(&use_block);

    // Enum bodies — populated by render_world via a placeholder
    // pass.  Placeholder is just an inline note here; the actual
    // emission is done at the call site to keep this helper free of
    // shim-package walking. We rely on the caller to splice the
    // enum block here.
    s.push_str("__SERDE_OPS_LOCAL_ENUMS_PLACEHOLDER__\n");

    // Record bodies — copy each record's field list verbatim. The
    // field types reference local-scope identifiers; references to
    // other primary records resolve in-interface (we declare all of
    // them here), and references to enums resolve against the
    // local enum copies inserted at the placeholder above.
    //
    // Records must be topologically ordered (used-before-defined
    // would be a WIT scope error). The walker preserves the source
    // declaration order, which the upstream WIT keeps in a sensible
    // bottom-up shape.  Phase E ships the registry order; if a
    // future shim needs explicit topo-sort, that's a follow-up.
    // #709: dedup by kebab. The registry keeps both entries when the
    // same kebab appears in two upstream interfaces (mobilitydb
    // `stbox3d`); the LOCAL serde-ops interface has ONE record per
    // kebab. Field-order differences between the upstream twins are
    // absorbed by CBOR's field-name encoding at the round-trip site.
    let mut seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let unique_records: Vec<&RecordType> = records
        .iter()
        .filter(|r| seen.insert(r.kebab_name.clone()))
        .collect();
    for r in &unique_records {
        s.push_str(&format!("    record {} {{\n", r.kebab_name));
        for (fname, ftype) in &r.fields {
            let resolved = inline_aliases(ftype, alias_map);
            s.push_str(&format!("        {fname}: {resolved},\n"));
        }
        s.push_str("    }\n");
    }
    s.push('\n');

    // Codec function declarations.
    for r in &unique_records {
        s.push_str(&format!(
            "    {name}-from-canon-cbor: func(bytes: list<u8>) -> result<{name}, string>;\n",
            name = r.kebab_name,
        ));
        s.push_str(&format!(
            "    {name}-to-canon-cbor: func(value: {name}) -> list<u8>;\n",
            name = r.kebab_name,
        ));
    }
    s.push_str("}\n");
    s
}

/// #785: source location of a WIT type identifier — used to emit
/// `use <ns>:<name>/<interface>@<ver>.{<type>};` at the top of the
/// bridge's local serde-ops interface for any identifier a record
/// field references but that the LOCAL clone paths (record dup,
/// enum clone, alias inline) don't cover.
#[derive(Debug, Clone)]
pub(crate) struct TypeSource {
    pub ns_name: String,
    pub version: String,
    pub interface: String,
    pub kind: TypeKind,
}

/// #785: WIT declaration kind for an identifier tracked in the type
/// source map. The kind drives whether the local serde-ops
/// interface duplicates the type verbatim (Enum, Record) or emits
/// a `use` directive to pull it in from the upstream interface
/// (Resource, Variant, Flags). Alias entries let the emitter skip
/// primary-package aliases (already inlined by `inline_aliases`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeKind {
    Resource,
    Variant,
    Flags,
    Record,
    Enum,
    Alias,
}

/// #785: build a map (kebab_name → TypeSource) covering every named
/// type declared in the discovered shim packages. Resource,
/// variant, flags, record, enum, and type-alias declarations all
/// register their kebab_name along with their `TypeKind` so the
/// downstream `use`-directive emitter can decide per-kind whether
/// the identifier needs a `use` line or resolves locally.
///
/// Collisions (same kebab in two different packages) keep the FIRST
/// entry seen — good enough for postgis's non-overlapping types.
/// mobilitydb's `stbox3d` collision is intra-package (two interfaces
/// in the same package), which the map absorbs without issue: both
/// entries land under the same kebab, and `render_use_directives`
/// filters records that are duplicated locally BEFORE consulting
/// the map so the local-record path always wins.
pub(crate) fn build_type_source_map(
    shim_packages: &[wit_parse::WitPackage],
) -> std::collections::BTreeMap<String, TypeSource> {
    let mut out: std::collections::BTreeMap<String, TypeSource> =
        std::collections::BTreeMap::new();
    for pkg in shim_packages {
        // Resources — the canonical non-cloneable case (opaque
        // handles like `geometry`, `raster`, `topology`).
        for r in &pkg.resources {
            out.entry(r.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: r.interface.clone(),
                kind: TypeKind::Resource,
            });
        }
        for v in &pkg.variants {
            out.entry(v.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: v.interface.clone(),
                kind: TypeKind::Variant,
            });
        }
        for f in &pkg.flags {
            out.entry(f.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: f.interface.clone(),
                kind: TypeKind::Flags,
            });
        }
        for r in &pkg.records {
            out.entry(r.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: r.interface.clone(),
                kind: TypeKind::Record,
            });
        }
        for e in &pkg.enums {
            out.entry(e.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: e.interface.clone(),
                kind: TypeKind::Enum,
            });
        }
        for a in &pkg.type_aliases {
            out.entry(a.kebab_name.clone()).or_insert(TypeSource {
                ns_name: pkg.ns_name.clone(),
                version: pkg.version.clone(),
                interface: a.interface.clone(),
                kind: TypeKind::Alias,
            });
        }
    }
    out
}

/// #785: emit `use <pkg-ns>/<interface>@<ver>.{<type>};` directives
/// covering every cross-interface identifier referenced by any
/// record field, that the LOCAL clone paths don't already cover.
///
/// Excluded from the emit (each has its own resolution path):
///   - WIT primitives / compound wrappers (never need `use`)
///   - Records duplicated locally in this interface (present in
///     `records`, distinguished by kebab_name)
///   - Primary-package enums cloned locally (spliced by
///     `render_world` via the enum placeholder)
///   - Primary-package type aliases (inlined by `inline_aliases`
///     when the field text is rendered)
///
/// Everything else — canonically resources like `geometry` — gets
/// a `use` directive so wit-bindgen resolves the reference.
///
/// Grouped by (ns_name, interface, version) so all types from one
/// upstream interface land in a single `use` line.
fn render_use_directives(
    records: &[RecordType],
    alias_map: &std::collections::BTreeMap<String, String>,
    type_source_map: &std::collections::BTreeMap<String, TypeSource>,
    primary: &str,
) -> String {
    // Records duplicated locally in serde-ops — never need `use`.
    let local_record_names: std::collections::BTreeSet<&str> =
        records.iter().map(|r| r.kebab_name.as_str()).collect();
    // Collect every non-local identifier referenced in a record
    // field's (alias-resolved) type text.
    let mut referenced: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for r in records {
        for (_fn, ftype) in &r.fields {
            let resolved = inline_aliases(ftype, alias_map);
            collect_type_idents(&resolved, &local_record_names, &mut referenced);
        }
    }
    // Group by (ns_name, interface, version).
    let mut groups: std::collections::BTreeMap<
        (String, String, String),
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for ident in &referenced {
        let Some(src) = type_source_map.get(ident) else {
            // Unresolvable identifier — the enum splicer or a truly
            // unknown ident. wit-bindgen surfaces the latter as a
            // diagnostic when it tries to resolve the identifier.
            continue;
        };
        // Kind-based routing.
        //
        // Primary-package types have three sub-cases:
        //   - Alias: inlined by `inline_aliases`. Never reaches
        //     this loop (the identifier is substituted out at the
        //     field-render site), but we defensively skip anyway.
        //   - Enum: cloned locally by the enum splicer in
        //     `render_world`. No `use` needed.
        //   - Resource / Variant / Flags / Record: NOT cloned
        //     locally — resources are opaque handles, variants and
        //     flags have complex bodies we don't duplicate, and
        //     any primary-package record NOT in `records` (e.g.
        //     filtered out by upstream because it's helper-package
        //     scoped) still needs a `use`.
        //
        // Non-primary types always emit `use` — helper-package
        // resources, records, variants, etc. all resolve through
        // the imported interface.
        if package_belongs_to_primary(&src.ns_name, primary) {
            match src.kind {
                TypeKind::Alias | TypeKind::Enum | TypeKind::Record => continue,
                TypeKind::Resource | TypeKind::Variant | TypeKind::Flags => {
                    // Fall through to emit `use`.
                }
            }
        }
        groups
            .entry((src.ns_name.clone(), src.interface.clone(), src.version.clone()))
            .or_default()
            .insert(ident.clone());
    }
    if groups.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for ((ns, iface, ver), names) in &groups {
        let joined = names
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "    use {ns}/{iface}@{ver}.{{{joined}}};\n"
        ));
    }
    out
}

/// Walk every record's field type-text and collect the names of any
/// type identifiers that resemble enum/variant references. Heuristic:
/// strip wrappers (list, option, borrow, tuple), then collect
/// identifiers that aren't WIT primitives or this set's record
/// names. Used by `render_serde_ops_interface` to decide which enums
/// need to be duplicated locally.
fn collect_referenced_enum_names(records: &[RecordType]) -> std::collections::BTreeSet<String> {
    let record_names: std::collections::BTreeSet<&str> =
        records.iter().map(|r| r.kebab_name.as_str()).collect();
    let mut out: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for r in records {
        for (_fn, ft) in &r.fields {
            collect_type_idents(ft, &record_names, &mut out);
        }
    }
    out
}

fn collect_type_idents(
    type_text: &str,
    record_names: &std::collections::BTreeSet<&str>,
    out: &mut std::collections::BTreeSet<String>,
) {
    // Split on punctuation that delimits identifiers in WIT type
    // exprs: `<`, `>`, `,`, `(`, `)`, whitespace.  Each resulting
    // chunk that's a kebab ident AND not a primitive AND not a
    // record-from-our-set is a candidate enum (or unrecognised
    // record from elsewhere; the consumer flags those).
    let mut buf = String::new();
    let mut chars = type_text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_alphanumeric() || c == '-' {
            buf.push(c);
        } else {
            flush_ident(&mut buf, record_names, out);
        }
        let _ = chars.peek();
    }
    flush_ident(&mut buf, record_names, out);
}

fn flush_ident(
    buf: &mut String,
    record_names: &std::collections::BTreeSet<&str>,
    out: &mut std::collections::BTreeSet<String>,
) {
    if buf.is_empty() {
        return;
    }
    let ident = std::mem::take(buf);
    if is_wit_primitive(&ident) {
        return;
    }
    if record_names.contains(ident.as_str()) {
        return;
    }
    // Filter out built-in compound markers (list, option, etc.)
    // since they appear as identifier-like chunks here.
    if matches!(
        ident.as_str(),
        "list" | "option" | "result" | "tuple" | "borrow"
    ) {
        return;
    }
    out.insert(ident);
}

fn is_wit_primitive(s: &str) -> bool {
    matches!(
        s,
        "bool"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "s8"
            | "s16"
            | "s32"
            | "s64"
            | "f32"
            | "f64"
            | "char"
            | "string"
    )
}

/// Inline-substitute every `type X = Y;` alias in `alias_map`
/// occurring as a standalone identifier inside `type_text`. The
/// substitution is fixpoint up to a small depth bound so chained
/// aliases (`type a = b; type b = c;`) collapse to the leaf type.
fn inline_aliases(
    type_text: &str,
    alias_map: &std::collections::BTreeMap<String, String>,
) -> String {
    if alias_map.is_empty() {
        return type_text.to_string();
    }
    let mut current = type_text.to_string();
    for _ in 0..4 {
        let next = substitute_idents(&current, alias_map);
        if next == current {
            return current;
        }
        current = next;
    }
    current
}

fn substitute_idents(
    s: &str,
    map: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ident = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            ident.push(c);
        } else {
            if !ident.is_empty() {
                if let Some(body) = map.get(&ident) {
                    out.push_str(body);
                } else {
                    out.push_str(&ident);
                }
                ident.clear();
            }
            out.push(c);
        }
    }
    if !ident.is_empty() {
        if let Some(body) = map.get(&ident) {
            out.push_str(body);
        } else {
            out.push_str(&ident);
        }
    }
    out
}

/// Render a local copy of an `enum` declaration. Used by
/// `render_world` after `render_serde_ops_interface` to splice in
/// the enum bodies the records reference. Phase E emits each enum
/// inside the serde-ops interface so the local records' field types
/// resolve.
fn render_local_enum(name: &str, cases: &[String]) -> String {
    let mut s = String::new();
    s.push_str(&format!("    enum {name} {{\n"));
    for (i, c) in cases.iter().enumerate() {
        let comma = if i + 1 < cases.len() { "," } else { "" };
        s.push_str(&format!("        {c}{comma}\n"));
    }
    s.push_str("    }\n");
    s
}

/// The fixed sqlite:extension import surface the host bindgen
/// targets. Phase D pulls these from the host wit dir's package
/// version dynamically; the interface list stays fixed at the
/// contract level.
pub const CONTRACT_IMPORTS: &[&str] =
    &["types", "spi", "logging", "config", "state", "cache"];

/// The fixed sqlite:extension export surface the host expects.
pub const CONTRACT_EXPORTS: &[&str] =
    &["metadata", "scalar-function", "aggregate-function", "vtab"];

pub(crate) fn primary_extension_name(plan: &BridgePlan) -> &str {
    plan.extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim")
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("source {} is not a directory", src.display()));
    }
    // #656: if `dst` is itself a symlink pointing back at a source WIT
    // tree, the umbrella-prune below would `unlink` files at the source
    // location (path traversal follows the dst symlink). Replace any dst
    // symlink with a real directory before continuing.
    if let Ok(meta) = fs::symlink_metadata(dst) {
        if meta.file_type().is_symlink() {
            fs::remove_file(dst).with_context(|| {
                format!("removing dst symlink {}", dst.display())
            })?;
        }
    }
    fs::create_dir_all(dst)?;
    // #642: when the upstream shim splits an umbrella `world.wit` into
    // per-interface .wit files, a stale `dst/world.wit` left over from
    // a previous regen still declares the same interfaces inline,
    // triggering a "duplicate item" parse error. Drop the stale file
    // before copying — if the source still owns a `world.wit`, the
    // loop below copies it right back; if not, it stays gone.
    // Use `symlink_metadata` so we don't follow a `world.wit` symlink.
    let stale_world = dst.join("world.wit");
    if fs::symlink_metadata(&stale_world).is_ok() {
        fs::remove_file(&stale_world)
            .with_context(|| format!("removing stale {}", stale_world.display()))?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_symlink() {
            // #623: resolve symlinks so generated bridges are
            // standalone (datafission per-extension wit/deps are
            // symlink farms; without this, the codegen silently
            // skipped them).
            let resolved = fs::canonicalize(&from)
                .with_context(|| format!("resolve symlink {}", from.display()))?;
            if resolved.is_dir() {
                copy_tree(&resolved, &to)?;
            } else if resolved.is_file() {
                if same_file(&resolved, &to) { continue; }
                copy_file_with_kebab_fix(&resolved, &to)?;
            }
        } else if file_type.is_file() {
            // Phase F (#522): when --out resolves to the SAME path
            // as the upstream shim's vendored wit/deps/ tree
            // (mobilitydb's default lookup hits its own wit/deps/),
            // fs::copy(src, src) truncates the file before reading.
            // Skip the copy in that case — the file is already
            // where it needs to be.
            if same_file(&from, &to) {
                continue;
            }
            copy_file_with_kebab_fix(&from, &to)?;
        }
        // skip symlinks / other — not expected in WIT trees
    }
    Ok(())
}

/// Copy `from` to `to`, applying the WIT kebab-fix
/// (`-2d` / `-3d` trailing-segment rewrite, #655) when the source is
/// a `.wit` file. Non-WIT files pass through `fs::copy` unchanged so
/// binary artifacts in a WIT tree (rare, but the codegen treats deps
/// trees opaquely) aren't corrupted by a text-mode round-trip.
fn copy_file_with_kebab_fix(from: &Path, to: &Path) -> Result<()> {
    let is_wit = from.extension().and_then(|s| s.to_str()) == Some("wit");
    if is_wit {
        let text = fs::read_to_string(from)
            .with_context(|| format!("read {}", from.display()))?;
        let fixed = kebab_fix_wit(&text);
        fs::write(to, fixed)
            .with_context(|| format!("write kebab-fixed {}", to.display()))?;
    } else {
        fs::copy(from, to)
            .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
    }
    Ok(())
}

/// True iff `a` and `b` refer to the same file once symlinks /
/// `..` segments are resolved. Falls back to a raw path-equality
/// check if canonicalisation fails (e.g. one of them doesn't
/// exist yet).
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

#[cfg(test)]
mod use_directive_tests {
    //! #785: `use` directives at the top of the serde-ops interface
    //! for cross-interface identifiers a record field references
    //! that the LOCAL clone paths don't cover (resources like
    //! `geometry`, cross-package variants/flags).
    use super::*;
    use datalink_shim_codegen_core::wit_parse::{
        WitEnumDecl, WitFlagsDecl, WitPackage, WitRecord, WitResourceDecl,
        WitTypeAlias, WitVariantDecl,
    };

    fn pkg(ns: &str, ver: &str) -> WitPackage {
        WitPackage {
            ns_name: ns.to_string(),
            version: ver.to_string(),
            interfaces: vec![],
            records: vec![],
            resources: vec![] as Vec<WitResourceDecl>,
            variants: vec![] as Vec<WitVariantDecl>,
            enums: vec![] as Vec<WitEnumDecl>,
            flags: vec![] as Vec<WitFlagsDecl>,
            type_aliases: vec![] as Vec<WitTypeAlias>,
        }
    }

    fn rec_type(
        pkg_ns: &str,
        pkg_ver: &str,
        interface: &str,
        kebab: &str,
        fields: &[(&str, &str)],
    ) -> RecordType {
        RecordType {
            package: pkg_ns.to_string(),
            package_version: pkg_ver.to_string(),
            interface: interface.to_string(),
            kebab_name: kebab.to_string(),
            fields: fields
                .iter()
                .map(|(n, t)| (n.to_string(), t.to_string()))
                .collect(),
            type_id: [0u8; 32],
            symbolic_name: format!(
                "{pkg_ns}@{pkg_ver}/{interface}/{kebab}"
            ),
            is_copy: false,
            direct: false,
            kebab_collides_in_pkg: false,
        }
    }

    /// #785 canonical trigger: `pixel-vec-entry.geom: geometry`
    /// where `geometry` is a resource declared in `postgis-types`.
    /// The emit MUST include `use postgis:wasm/postgis-types@0.1.0.{geometry};`
    /// so wit-bindgen resolves the identifier.
    #[test]
    fn use_directive_emitted_for_resource_referenced_by_record() {
        let mut postgis = pkg("postgis:wasm", "0.1.0");
        postgis.resources.push(WitResourceDecl {
            interface: "postgis-types".to_string(),
            kebab_name: "geometry".to_string(),
        });
        let records = vec![rec_type(
            "postgis:wasm",
            "0.1.0",
            "postgis-raster-vector",
            "pixel-vec-entry",
            &[
                ("geom", "geometry"),
                ("val", "f64"),
                ("x", "u32"),
                ("y", "u32"),
            ],
        )];
        let alias_map = std::collections::BTreeMap::new();
        let type_source_map = build_type_source_map(&[postgis]);
        let out = render_serde_ops_interface(
            &records,
            &alias_map,
            &type_source_map,
            "postgis",
        );
        assert!(
            out.contains(
                "use postgis:wasm/postgis-types@0.1.0.{geometry};"
            ),
            "expected `use postgis:wasm/postgis-types@0.1.0.{{geometry}};` in \
             emitted serde-ops interface, got:\n{out}"
        );
        // Sanity: record itself still declared locally.
        assert!(out.contains("record pixel-vec-entry {"));
        assert!(out.contains("geom: geometry"));
    }

    /// Multiple cross-interface identifiers from the same upstream
    /// interface collapse into a single `use` line with a
    /// comma-separated brace list.
    #[test]
    fn use_directive_groups_multiple_types_from_same_interface() {
        let mut postgis = pkg("postgis:wasm", "0.1.0");
        postgis.resources.push(WitResourceDecl {
            interface: "postgis-types".to_string(),
            kebab_name: "geometry".to_string(),
        });
        postgis.resources.push(WitResourceDecl {
            interface: "postgis-types".to_string(),
            kebab_name: "geography".to_string(),
        });
        let records = vec![rec_type(
            "postgis:wasm",
            "0.1.0",
            "some-iface",
            "geo-pair",
            &[("g1", "geometry"), ("g2", "geography")],
        )];
        let alias_map = std::collections::BTreeMap::new();
        let type_source_map = build_type_source_map(&[postgis]);
        let out = render_serde_ops_interface(
            &records,
            &alias_map,
            &type_source_map,
            "postgis",
        );
        assert!(
            out.contains(
                "use postgis:wasm/postgis-types@0.1.0.{geography, geometry};"
            ),
            "expected grouped `use` line, got:\n{out}"
        );
    }

    /// Primary-package aliases are inlined by `inline_aliases`; they
    /// must NOT show up in a `use` directive. Regression guard so a
    /// future change to the alias path doesn't accidentally re-route
    /// them through the `use` emitter.
    #[test]
    fn use_directive_skipped_for_primary_alias_type() {
        let mut postgis = pkg("postgis:wasm", "0.1.0");
        postgis.type_aliases.push(WitTypeAlias {
            interface: "postgis-types".to_string(),
            kebab_name: "srid".to_string(),
            body: "s32".to_string(),
        });
        let records = vec![rec_type(
            "postgis:wasm",
            "0.1.0",
            "postgis-types",
            "srid-record",
            &[("s", "srid")],
        )];
        let mut alias_map = std::collections::BTreeMap::new();
        alias_map.insert("srid".to_string(), "s32".to_string());
        let type_source_map = build_type_source_map(&[postgis]);
        let out = render_serde_ops_interface(
            &records,
            &alias_map,
            &type_source_map,
            "postgis",
        );
        assert!(
            !out.contains("use postgis:wasm/postgis-types@0.1.0.{srid}"),
            "primary-alias `srid` should not appear in a `use` directive, got:\n{out}"
        );
        // Alias was inlined — field text should be `s: s32`.
        assert!(out.contains("s: s32"), "expected inlined alias, got:\n{out}");
    }

    /// Records referenced by other records that are duplicated
    /// locally must NOT emit `use` directives — the referenced
    /// record lands in the same interface and resolves in-scope.
    #[test]
    fn use_directive_skipped_for_locally_duplicated_record() {
        let postgis = pkg("postgis:wasm", "0.1.0");
        let records = vec![
            rec_type(
                "postgis:wasm",
                "0.1.0",
                "postgis-types",
                "coord",
                &[("x", "f64"), ("y", "f64")],
            ),
            rec_type(
                "postgis:wasm",
                "0.1.0",
                "postgis-types",
                "extremes",
                &[
                    ("x-min-point", "coord"),
                    ("x-max-point", "coord"),
                ],
            ),
        ];
        let alias_map = std::collections::BTreeMap::new();
        let type_source_map = build_type_source_map(&[postgis]);
        let out = render_serde_ops_interface(
            &records,
            &alias_map,
            &type_source_map,
            "postgis",
        );
        assert!(
            !out.contains("use postgis:wasm/postgis-types@0.1.0.{coord}"),
            "locally-duplicated record `coord` should not appear in a `use` directive, got:\n{out}"
        );
    }
}
