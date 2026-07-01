//! Phase C — per-shim WIT record-type registry.
//!
//! Walks every `WitPackage` discovered by `emit_wit::discover_shim_packages`
//! and produces a `RecordType` per `record NAME { ... }` block. Each
//! entry carries:
//!
//!   - `package` (e.g. `mobilitydb:temporal`),
//!   - `package_version` (e.g. `0.1.0`),
//!   - `interface` (the WIT interface name the record was declared in,
//!     e.g. `types`),
//!   - `kebab_name` (e.g. `tfloat-sequence`),
//!   - `fields` (ordered list of `(kebab field name, raw type text)`),
//!   - `type_id` — 32-byte sha256 over a canonical text form
//!     (`witcanon:1\n<package>/<interface>/<record>\n<sorted fields>\n`),
//!   - `symbolic_name` — `<package>@<version>/<interface>/<record>`.
//!
//! Downstream:
//!   - `emit_lib::lib_rs` consults the registry to emit per-record
//!     stub encoder/decoder bodies on the `serde-ops` export.
//!   - `dispatch.rs` consults the registry to detect record-typed
//!     param/return signatures and dispatch through wit-value.
//!   - `emit_metadata_impl` emits one `typed-value-binding` entry per
//!     record actually referenced by a dispatched arm.

use sha2::{Digest, Sha256};

use crate::wit_parse::WitPackage;

/// One entry in the per-shim record-type registry.
#[derive(Debug, Clone)]
pub struct RecordType {
    pub package: String,
    pub package_version: String,
    pub interface: String,
    pub kebab_name: String,
    pub fields: Vec<(String, String)>,
    pub type_id: [u8; 32],
    pub symbolic_name: String,
    /// True when all of the record's fields' types resolve to
    /// types that wit-bindgen generates Rust `Copy` impls for
    /// (all primitives, primitive-only nested records, enums).
    /// Drives the dispatch arm's pass-by-value vs pass-by-ref
    /// decision: `wit-bindgen` generates `fn f(seq: &Record)` for
    /// non-Copy record types and `fn f(seq: Record)` for Copy ones.
    pub is_copy: bool,
    /// Task #523: true when the LOCAL serde-ops Rust type and the
    /// UPSTREAM Rust type are byte-compatible under serde's CBOR
    /// codec (ciborium). When `direct == true`, the dispatch arm
    /// can skip the LOCAL→UPSTREAM ciborium round-trip and decode
    /// payload bytes directly into the upstream type.
    ///
    /// Criterion: every field's type, after WIT alias resolution,
    /// must resolve to one of:
    ///   - a WIT primitive (transparent at the Rust serde layer),
    ///   - a same-package type alias whose chain ends at a
    ///     primitive (wit-bindgen emits transparent `pub type`),
    ///   - a same-package enum (LOCAL clones it verbatim with the
    ///     same variant names + order; default serde encodes
    ///     unit enums by variant name → CBOR-identical),
    ///   - a same-package record that is itself `direct`.
    /// Cross-package references force `direct = false` because
    /// the LOCAL doesn't clone cross-package types and the WIT
    /// would fail to typecheck.
    pub direct: bool,
    /// #710: true when at least one OTHER record in the same
    /// package shares this record's kebab_name (e.g. mobilitydb
    /// declares `record stbox3d` in both `stbox-ops` and
    /// `stbox3d-ops`). Downstream helper naming (`arg_witvalue_*`
    /// / `ret_to_witvalue_*` / `parse_json_list_record_*`) prepends
    /// the snake-case interface name so the two variants get
    /// distinct wrapper functions. See `helper_snake()`.
    pub kebab_collides_in_pkg: bool,
}

impl RecordType {
    /// `<package>@<version>/<interface>/<record>` symbolic name as
    /// used in `typed-value-binding.symbolic-name` and in
    /// diagnostics. Example:
    ///   `"mobilitydb:temporal@0.1.0/types/tfloat-sequence"`.
    pub fn symbolic_name_for(
        package: &str,
        version: &str,
        interface: &str,
        kebab: &str,
    ) -> String {
        format!("{package}@{version}/{interface}/{kebab}")
    }

    /// Snake_case Rust ident for the record (e.g. `tfloat-sequence`
    /// → `tfloat_sequence`). Drives the encoder/decoder Rust function
    /// names emitted into the bridge.
    pub fn snake_name(&self) -> String {
        self.kebab_name.replace('-', "_")
    }

    /// #710: helper-function suffix. Same as `snake_name()` when the
    /// record's kebab is unique within its package; when the kebab
    /// collides (e.g. mobilitydb's `stbox3d` defined in two
    /// interfaces with different field orders), the interface's
    /// snake form is prepended so each variant gets its own
    /// `arg_witvalue_*` / `ret_to_witvalue_*` / codec helper.
    /// Uses a single `_` separator (rustc's snake_case lint rejects
    /// `__`) — the shape `<iface_snake>_<kebab_snake>` is
    /// unambiguous because interface names never contain underscores
    /// (WIT is kebab-case only).
    pub fn helper_snake(&self) -> String {
        let kebab = self.kebab_name.replace('-', "_");
        if self.kebab_collides_in_pkg {
            let iface = self.interface.replace('-', "_");
            format!("{iface}_{kebab}")
        } else {
            kebab
        }
    }

    /// Encoder import name as it appears in
    /// `typed-value-binding.encoder-import`. Mirrors the
    /// `<package>:wasm/serde-ops/<type>-to-canon-cbor` convention.
    /// Note: the package portion is taken VERBATIM from the WIT
    /// package, so `mobilitydb:temporal/serde-ops/tfloat-sequence-to-canon-cbor`.
    pub fn encoder_import(&self) -> String {
        format!(
            "sqlink-bridge:{}/serde-ops/{}-to-canon-cbor",
            primary_from_pkg(&self.package),
            self.kebab_name,
        )
    }

    pub fn decoder_import(&self) -> String {
        format!(
            "sqlink-bridge:{}/serde-ops/{}-from-canon-cbor",
            primary_from_pkg(&self.package),
            self.kebab_name,
        )
    }
}

/// Build the registry from the per-shim packages. Records found in
/// the host-loader contract package are filtered out — the contract
/// records (e.g. `wit-value-payload`) aren't bridge-side serde-ops
/// targets.
///
/// Phase E: records are DEDUPLICATED by kebab name within the same
/// package. WIT lets the same record name appear in two interfaces
/// (different scopes), but the bridge's local serde-ops interface
/// folds them into a single namespace — keeping both would
/// double-declare. First occurrence wins; later collisions get
/// dropped silently (their dispatch arms continue to reference the
/// first record's type-id).
///
/// Task #523: each record's `direct` flag is computed via fix-point
/// against the primary shim's enum + alias environment. `primary`
/// is the shim's ns_name prefix (e.g. `"mobilitydb"` or `"postgis"`)
/// — used to scope same-package checks for the structural-identity
/// short-circuit.
pub fn build(shim_packages: &[WitPackage], primary: &str) -> Vec<RecordType> {
    let mut out: Vec<RecordType> = Vec::new();
    // #710: dedupe on `(interface, kebab)` rather than just `kebab`,
    // so a shim that declares the same kebab record in two interfaces
    // (mobilitydb-temporal defines `record stbox3d` in both
    // `stbox-ops` and `stbox3d-ops` with different field order)
    // preserves both variants. Same-interface duplicates still fold
    // to one — wit-bindgen would refuse to emit two Rust types with
    // the same name in one module anyway.
    let mut seen_per_pkg: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<(String, String)>,
    > = std::collections::BTreeMap::new();
    for pkg in shim_packages {
        if pkg.ns_name == "sqlite:extension" {
            continue;
        }
        let seen = seen_per_pkg
            .entry(pkg.ns_name.clone())
            .or_default();
        for r in &pkg.records {
            if !seen.insert((r.interface.clone(), r.kebab_name.clone())) {
                continue;
            }
            let symbolic = RecordType::symbolic_name_for(
                &pkg.ns_name,
                &pkg.version,
                &r.interface,
                &r.kebab_name,
            );
            let type_id = canonical_type_id(
                &pkg.ns_name,
                &r.interface,
                &r.kebab_name,
                &r.fields,
            );
            out.push(RecordType {
                package: pkg.ns_name.clone(),
                package_version: pkg.version.clone(),
                interface: r.interface.clone(),
                kebab_name: r.kebab_name.clone(),
                fields: r.fields.clone(),
                type_id,
                symbolic_name: symbolic,
                // Computed below in a fix-point pass once the
                // registry is complete.
                is_copy: false,
                // Computed below in a separate fix-point pass.
                // Non-primary-package records never short-circuit
                // (the bridge doesn't own their codecs), so we
                // skip them outright.
                direct: false,
                // Computed below once the registry is fully populated
                // (needs a per-package count of records with the
                // same kebab).
                kebab_collides_in_pkg: false,
            });
        }
    }
    // #710: mark records whose kebab appears more than once in the
    // same package so `helper_snake()` disambiguates their emitted
    // helper names. Postgis: no collisions today so this is a no-op.
    // mobilitydb: `stbox3d` collides across `stbox-ops` and
    // `stbox3d-ops`.
    let mut kebab_counts: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    for r in &out {
        *kebab_counts
            .entry((r.package.clone(), r.kebab_name.clone()))
            .or_insert(0) += 1;
    }
    for r in &mut out {
        if let Some(&n) = kebab_counts.get(&(r.package.clone(), r.kebab_name.clone())) {
            if n > 1 {
                r.kebab_collides_in_pkg = true;
            }
        }
    }
    // Fix-point Copy analysis. A record is Copy iff every field's
    // type resolves to Copy in Rust's wit-bindgen output:
    // primitives, enums (assumed simple-tag), `option<T>` over
    // Copy T, and other-Copy records. `string`, `list<T>`,
    // `variant<...>`, and any unknown type force non-Copy.
    let known_names: std::collections::BTreeSet<String> =
        out.iter().map(|r| r.kebab_name.clone()).collect();
    loop {
        let mut changed = false;
        for i in 0..out.len() {
            if out[i].is_copy {
                continue;
            }
            let all_copy = out[i]
                .fields
                .iter()
                .all(|(_, ft)| field_type_is_copy(ft, &out, &known_names));
            if all_copy {
                out[i].is_copy = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Task #523: fix-point structural-identity analysis. Compute
    // `direct` only for records whose package is the primary shim's
    // own package (non-primary records never short-circuit because
    // the bridge doesn't generate a LOCAL clone of them).
    //
    // The primary-package alias table + enum set drive the per-field
    // identifier resolution: an identifier is "direct-friendly" iff
    // it resolves to a WIT primitive, a same-package alias to a
    // direct-friendly type, a same-package enum (cloned verbatim in
    // LOCAL → same serde tag), or a same-package record that is
    // itself `direct`.
    let primary_alias_resolutions = build_primary_alias_resolutions(shim_packages, primary);
    let primary_enum_names: std::collections::BTreeSet<String> = shim_packages
        .iter()
        .filter(|p| ns_belongs_to_primary(&p.ns_name, primary))
        .flat_map(|p| p.enums.iter())
        .map(|e| e.kebab_name.clone())
        .collect();
    // Only primary-package records are candidates.
    loop {
        let mut changed = false;
        for i in 0..out.len() {
            if out[i].direct {
                continue;
            }
            if !ns_belongs_to_primary(&out[i].package, primary) {
                continue;
            }
            let all_direct = out[i].fields.iter().all(|(_, ft)| {
                field_type_is_direct(
                    ft,
                    primary,
                    &out,
                    &primary_enum_names,
                    &primary_alias_resolutions,
                )
            });
            if all_direct {
                out[i].direct = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// Lower-cased primary-prefix membership check. Mirrors
/// `emit_wit::package_belongs_to_primary` so the registry doesn't
/// need to import emit_wit.
fn ns_belongs_to_primary(package: &str, primary: &str) -> bool {
    package
        .split(':')
        .next()
        .map(|ns| ns == primary)
        .unwrap_or(false)
}

/// Build a map of primary-package type aliases keyed by kebab name.
/// Used by `field_type_is_direct` to resolve through aliases on the
/// way down to primitives / enums / records.
fn build_primary_alias_resolutions(
    shim_packages: &[WitPackage],
    primary: &str,
) -> std::collections::BTreeMap<String, String> {
    shim_packages
        .iter()
        .filter(|p| ns_belongs_to_primary(&p.ns_name, primary))
        .flat_map(|p| p.type_aliases.iter())
        .map(|a| (a.kebab_name.clone(), a.body.clone()))
        .collect()
}

/// Per-field walk used by the `direct` fix-point. Returns true when
/// the WIT type-text resolves entirely to identifiers that
/// CBOR-encode identically between the LOCAL clone and the UPSTREAM
/// Rust types. See `RecordType::direct` doc for the criterion.
fn field_type_is_direct(
    type_text: &str,
    primary: &str,
    records: &[RecordType],
    primary_enums: &std::collections::BTreeSet<String>,
    primary_aliases: &std::collections::BTreeMap<String, String>,
) -> bool {
    let t = type_text.trim();
    // Empty / unparseable — conservative bail.
    if t.is_empty() {
        return false;
    }
    // Compound wrappers — recurse into the inner type.
    if let Some(inner) = strip_wrapper(t, "option<") {
        return field_type_is_direct(&inner, primary, records, primary_enums, primary_aliases);
    }
    if let Some(inner) = strip_wrapper(t, "list<") {
        return field_type_is_direct(&inner, primary, records, primary_enums, primary_aliases);
    }
    if let Some(inner) = strip_wrapper(t, "borrow<") {
        return field_type_is_direct(&inner, primary, records, primary_enums, primary_aliases);
    }
    if t.starts_with("tuple<") {
        // Tuples don't appear in primary-shim records as of #523's
        // mobilitydb corpus. Conservative bail keeps us from
        // claiming structural identity over a shape we haven't
        // verified.
        return false;
    }
    if t.starts_with("result<") {
        // Same conservative bail.
        return false;
    }
    // Primitive — Rust generates `i64`, `f64`, etc. identically on
    // both sides.
    match t {
        "bool" | "u8" | "u16" | "u32" | "u64" | "s8" | "s16" | "s32" | "s64"
        | "f32" | "f64" | "char" | "string" => return true,
        _ => {}
    }
    // Identifier — must resolve within the primary-package universe.
    // (1) Same-package record that is itself `direct`.
    if let Some(rec) = records.iter().find(|r| r.kebab_name == t) {
        if !ns_belongs_to_primary(&rec.package, primary) {
            return false;
        }
        return rec.direct;
    }
    // (2) Same-package enum — LOCAL clones it with the same variant
    // names + order, so default-serde tags it identically.
    if primary_enums.contains(t) {
        return true;
    }
    // (3) Same-package type alias — resolve through and recurse.
    if let Some(body) = primary_aliases.get(t) {
        return field_type_is_direct(body, primary, records, primary_enums, primary_aliases);
    }
    // Unknown identifier — possibly cross-package or a flag/variant
    // / non-cloneable type. Conservative bail.
    false
}

/// Return true when the WIT field-type text resolves to a Rust
/// Copy type as wit-bindgen generates it. Handles primitives,
/// `option<T>`, references to records-known-to-be-Copy, and
/// assumes any name that doesn't resolve as a record IS a
/// simple-tag enum (Copy).  Strings, lists, and tuples force
/// non-Copy.
fn field_type_is_copy(
    type_text: &str,
    records: &[RecordType],
    known_names: &std::collections::BTreeSet<String>,
) -> bool {
    let t = type_text.trim();
    if t.starts_with("string") {
        return false;
    }
    if t.starts_with("list<") {
        return false;
    }
    if t.starts_with("tuple<") {
        return false;
    }
    if let Some(inner) = strip_wrapper(t, "option<") {
        return field_type_is_copy(&inner, records, known_names);
    }
    if let Some(inner) = strip_wrapper(t, "borrow<") {
        // `borrow<T>` is a reference; the inner type's Copy-ness
        // doesn't affect the field's Copy-ness, but borrow itself
        // is a reference which IS Copy. Conservatively treat
        // borrow as Copy.
        let _ = inner;
        return true;
    }
    match t {
        "bool" | "u8" | "u16" | "u32" | "u64" | "s8" | "s16" | "s32" | "s64"
        | "f32" | "f64" | "char" => true,
        _ => {
            // Maybe a referenced record name.
            if let Some(rec) = records.iter().find(|r| r.kebab_name == t) {
                return rec.is_copy;
            }
            // Unknown identifier: assume enum (Copy). Conservative
            // but matches wit-bindgen's behaviour for primary-shim
            // enums which are simple-tag and Copy.
            if known_names.contains(t) {
                return false; // record we know about but not yet Copy
            }
            true
        }
    }
}

fn strip_wrapper(s: &str, open: &str) -> Option<String> {
    if !s.starts_with(open) {
        return None;
    }
    let rest = &s[open.len()..];
    if !rest.ends_with('>') {
        return None;
    }
    Some(rest[..rest.len() - 1].to_string())
}

/// Canonical-WIT type-id: 32-byte sha256 over a deterministic text
/// form. The form is intentionally simple (Phase C):
///
/// ```text
/// witcanon:1\n
/// <package>/<interface>/<record>\n
/// <field>:<type>\n
/// <field>:<type>\n
/// ...
/// ```
///
/// Field lines are SORTED by field name so the same record shape
/// hashes identically regardless of source-text field order. This is
/// the minimum-viable normalization Phase C ships; PLAN's "canon:wit"
/// (#486) replaces it with a richer canonical form once the
/// orchestration substrate lands.
fn canonical_type_id(
    package: &str,
    interface: &str,
    kebab: &str,
    fields: &[(String, String)],
) -> [u8; 32] {
    let mut lines: Vec<String> =
        fields.iter().map(|(n, t)| format!("{n}:{t}")).collect();
    lines.sort();
    let mut input = String::new();
    input.push_str("witcanon:1\n");
    input.push_str(&format!("{package}/{interface}/{kebab}\n"));
    for l in &lines {
        input.push_str(l);
        input.push('\n');
    }
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out[..]);
    arr
}

fn primary_from_pkg(pkg: &str) -> String {
    // Per the manifest convention "sqlink-bridge:<primary>/serde-ops/...",
    // the primary segment is the package namespace (the bit before
    // the colon). For "mobilitydb:temporal" that's "mobilitydb".
    pkg.split(':').next().unwrap_or(pkg).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wit_parse::{
        WitEnumDecl, WitFlagsDecl, WitPackage, WitRecord, WitResourceDecl,
        WitTypeAlias, WitVariantDecl,
    };

    fn pkg(ns: &str, ver: &str) -> WitPackage {
        WitPackage {
            ns_name: ns.to_string(),
            version: ver.to_string(),
            interfaces: vec!["types".to_string()],
            records: vec![],
            resources: vec![] as Vec<WitResourceDecl>,
            variants: vec![] as Vec<WitVariantDecl>,
            enums: vec![] as Vec<WitEnumDecl>,
            flags: vec![] as Vec<WitFlagsDecl>,
            type_aliases: vec![] as Vec<WitTypeAlias>,
        }
    }
    fn rec(iface: &str, name: &str, fields: &[(&str, &str)]) -> WitRecord {
        WitRecord {
            interface: iface.to_string(),
            kebab_name: name.to_string(),
            fields: fields
                .iter()
                .map(|(n, t)| (n.to_string(), t.to_string()))
                .collect(),
        }
    }

    /// All-primitive record short-circuits.
    #[test]
    fn direct_all_primitives() {
        let mut p = pkg("mobilitydb:temporal", "0.1.0");
        p.records.push(rec(
            "types",
            "time-period",
            &[("start", "s64"), ("end", "s64"), ("inc", "bool")],
        ));
        let registry = build(&[p], "mobilitydb");
        assert_eq!(registry.len(), 1);
        assert!(registry[0].direct, "all-primitive record must be direct");
    }

    /// Type-alias to a primitive resolves through.
    #[test]
    fn direct_through_primitive_alias() {
        let mut p = pkg("mobilitydb:temporal", "0.1.0");
        p.type_aliases.push(WitTypeAlias {
            interface: "types".to_string(),
            kebab_name: "timestamp-tz".to_string(),
            body: "s64".to_string(),
        });
        p.records.push(rec(
            "types",
            "tfloat-instant",
            &[("timestamp", "timestamp-tz"), ("value", "f64")],
        ));
        let registry = build(&[p], "mobilitydb");
        assert!(
            registry[0].direct,
            "field with primitive-alias type must short-circuit"
        );
    }

    /// Same-package enum is cloned verbatim → CBOR-identical.
    #[test]
    fn direct_with_same_package_enum() {
        let mut p = pkg("mobilitydb:temporal", "0.1.0");
        p.enums.push(WitEnumDecl {
            interface: "types".to_string(),
            kebab_name: "interpolation".to_string(),
            cases: vec!["stepwise".to_string(), "linear".to_string()],
        });
        p.records.push(rec(
            "types",
            "tfloat-sequence",
            &[
                ("interpolation", "interpolation"),
                ("lower-inclusive", "bool"),
            ],
        ));
        let registry = build(&[p], "mobilitydb");
        assert!(
            registry[0].direct,
            "same-package enum reference must short-circuit"
        );
    }

    /// Nested same-package record propagates direct-ness.
    #[test]
    fn direct_with_nested_record() {
        let mut p = pkg("mobilitydb:temporal", "0.1.0");
        p.records.push(rec(
            "types",
            "tfloat-instant",
            &[("timestamp", "s64"), ("value", "f64")],
        ));
        p.records.push(rec(
            "types",
            "tfloat-sequence",
            &[("instants", "list<tfloat-instant>")],
        ));
        let registry = build(&[p], "mobilitydb");
        let seq = registry.iter().find(|r| r.kebab_name == "tfloat-sequence").unwrap();
        assert!(seq.direct, "nested same-package record propagates direct");
    }

    /// Cross-package identifier blocks short-circuit.
    #[test]
    fn nondirect_with_cross_package_reference() {
        // Helper package has a record `helper-blob`; primary
        // references it. LOCAL doesn't clone cross-pkg types, so
        // the WIT wouldn't even typecheck — `direct` must be false.
        let mut helper = pkg("sfcgal:component", "0.1.0");
        helper.records.push(rec("ops", "helper-blob", &[("len", "u32")]));
        let mut primary = pkg("postgis:wasm", "0.1.0");
        primary.records.push(rec(
            "types",
            "weird-record",
            &[("h", "helper-blob")],
        ));
        let registry = build(&[helper, primary], "postgis");
        let weird = registry
            .iter()
            .find(|r| r.kebab_name == "weird-record")
            .unwrap();
        assert!(
            !weird.direct,
            "cross-package field reference must block short-circuit"
        );
    }

    /// Unsupported wrapper (tuple, result) blocks short-circuit.
    #[test]
    fn nondirect_with_tuple_or_result_field() {
        let mut p = pkg("mobilitydb:temporal", "0.1.0");
        p.records.push(rec(
            "types",
            "weird-record",
            &[("pair", "tuple<s64, s64>")],
        ));
        let registry = build(&[p], "mobilitydb");
        assert!(!registry[0].direct, "tuple field must block direct");
    }

    /// Non-primary records always stay false even when their
    /// fields are all primitives — the bridge doesn't clone them.
    #[test]
    fn non_primary_records_never_direct() {
        let mut helper = pkg("sfcgal:component", "0.1.0");
        helper.records.push(rec("ops", "helper-blob", &[("len", "u32")]));
        let registry = build(&[helper], "postgis");
        assert_eq!(registry.len(), 1);
        assert!(
            !registry[0].direct,
            "non-primary record never short-circuits even if primitive-only"
        );
    }
}
