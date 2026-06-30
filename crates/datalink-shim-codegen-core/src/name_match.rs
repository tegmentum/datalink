//! SQL-side ↔ WIT-side name matching.
//!
//! The interface DB stores SQL names in "no-underscore" form
//! (`st_makepoint`); the WIT uses "underscored long form"
//! (`st-make-point` → `st_make_point`). Aliases bridge the two
//! forms. This module owns:
//!
//! - candidate-list generation per SQL function (with similarity
//!   sorting against the canonical name),
//! - Levenshtein edit-distance helper,
//! - the snake / no-hyphen / resource-method indexes the
//!   dispatcher walks,
//! - lookup helpers (`find_wit_fn`, `find_resource_method`,
//!   `find_same_interface_free_fn`) that ride those indexes in
//!   priority order,
//! - alias collection (`type X = Y`) + enum collection (per
//!   primary shim) + alias-resolution pass over a `Vec<WitFunction>`.
//!
//! Everything here is pure-IR: no `SqlValue`, no Rust-code
//! emission. Step 2 of #566 lifts this module into
//! `datalink-shim-codegen-core` verbatim.

use std::collections::HashMap;
use std::path::Path;

use crate::wit_parse::{
    self, WitEnumDecl, WitFunction, WitParam, WitRet, WitTypeAlias,
};
use shim_bridge_codegen_core::{AggregateFn, ScalarFn, TableFn};

/// Suffix tokens stripped from a SQL scalar name as a last-resort
/// fallback when no WIT function matches the canonical or alias
/// forms. Centred on mobilitydb's variant surface; extend as new
/// shims add their own naming idioms.
pub const SCALAR_NAME_SUFFIXES: &[&str] = &["_wit", "_scalar"];

/// Like `SCALAR_NAME_SUFFIXES` but for aggregate names. mobilitydb
/// duplicates a handful of scalar functions under `<name>_agg`
/// for the SQL aggregate slot — the underlying WIT function is the
/// bare-name scalar.
pub const AGGREGATE_NAME_SUFFIXES: &[&str] = &["_agg", "_aggregate"];

/// W3.3 (#543): a WIT `enum` decl paired with its owning package.
/// `WitEnumDecl` carries the interface but not the package; pairing
/// at collection time lets the dispatcher emit the right
/// `use bindings::<ns>::<name>::<module>` path for each enum.
#[derive(Debug, Clone)]
pub struct EnumWithPackage {
    pub package: String,
    pub decl: WitEnumDecl,
}

/// Compute the SQL-name candidates the codegen will try to look
/// up against the WIT index. The interface DB stores names in
/// "no-underscore" form (`st_makepoint`); the WIT uses
/// "underscored long form" (`st-make-point` → `st_make_point`).
/// Aliases bridge the two forms.
pub fn sql_name_candidates(sc: &ScalarFn) -> Vec<String> {
    candidates_sorted(&sc.canonical_name, &sc.aliases)
}

pub fn aggregate_name_candidates(ag: &AggregateFn) -> Vec<String> {
    candidates_sorted(&ag.canonical_name, &ag.aliases)
}

pub fn table_fn_name_candidates(tf: &TableFn) -> Vec<String> {
    candidates_sorted(&tf.canonical_name, &tf.aliases)
}

/// Build the candidate list `[canonical, ...aliases]` with the
/// aliases sorted by string similarity to the canonical. The
/// interface DB's alias table sometimes lists semantically-
/// unrelated names (e.g. `st_as_marc21` IS listed as an alias of
/// `st_astext`). Sorting by similarity means the underscored
/// form of the canonical (`st_as_text`) gets matched against the
/// WIT before the unrelated alias does, eliminating the wrong-
/// dispatch hazard.
pub fn candidates_sorted(canonical: &str, aliases: &[String]) -> Vec<String> {
    let mut v = Vec::with_capacity(1 + aliases.len());
    v.push(canonical.to_string());
    let mut scored: Vec<(String, usize)> = aliases
        .iter()
        .map(|a| (a.clone(), edit_distance(canonical, a)))
        .collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    for (a, _) in scored {
        v.push(a);
    }
    v
}

/// Levenshtein distance between two ASCII strings. The PostGIS
/// surface uses only ASCII identifiers so byte-wise comparison
/// is correct.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Build a `snake_case_name → WitFunction` index over every
/// recognised WIT FREE function so the SQL-side lookup is O(1).
/// #547 (W3.1): resource methods (`f.resource.is_some()`) are
/// excluded so the prefix-strip lookup (e.g. `srid` → matches
/// `topology.srid()`) can't confuse a method for a free function;
/// methods go through `index_resource_methods` instead.
/// #556 (W3.1 mop-up): resource CONSTRUCTORS are kept in this index
/// (kebab `create-<resource>`) — the SQL surface calls them by
/// free-function-shaped names like `st_createtopology`, so they
/// resolve through the standard snake / no-hyphen lookups.
pub fn index_wit_fns(fns: &[WitFunction]) -> HashMap<String, &WitFunction> {
    let mut idx = HashMap::with_capacity(fns.len());
    for f in fns {
        if f.resource.is_some() && !f.is_constructor {
            continue;
        }
        idx.insert(wit_parse::kebab_to_snake(&f.kebab_name), f);
    }
    idx
}

/// #547 (W3.1): per-resource method index. Keys are
/// `<resource_kebab_snake>_<method_kebab_snake>` so SQL candidates
/// like `topology_node_count` or `topology_to_bytes` resolve
/// directly. Free functions are skipped here (covered by
/// `index_wit_fns`).
///
/// The interface DB stores SQL scalar names like `st_topologynodecount`
/// (no separator); `sql_name_candidates` already produces
/// `topology_node_count` as an alias, so the lookup is one HashMap
/// hit after `find_wit_fn` misses.
pub fn index_resource_methods(fns: &[WitFunction]) -> HashMap<String, &WitFunction> {
    let mut idx = HashMap::new();
    for f in fns {
        let Some(ref rkebab) = f.resource else {
            continue;
        };
        // #556 (W3.1 mop-up): constructors are routed via
        // `index_wit_fns` (free-function-shape lookup). Skipping
        // here keeps the `<resource>_<method>` keys reserved for
        // instance methods — otherwise a SQL alias like
        // `topology_create_topology` could collide with a future
        // `topology_create` method.
        if f.is_constructor {
            continue;
        }
        let key = format!(
            "{}_{}",
            wit_parse::kebab_to_snake(rkebab),
            wit_parse::kebab_to_snake(&f.kebab_name),
        );
        idx.insert(key, f);
    }
    idx
}

/// Round-490: secondary "no-hyphen kebab" index. Each WIT function
/// is keyed by its kebab name with hyphens removed, e.g. `add-face`
/// → `addface`, `as-topojson` → `astopojson`. This catches SQL
/// names whose underscored long form doesn't exist as an alias —
/// the interface DB writes `st_addface` (no separator), and after
/// stripping `st_` we want `addface` to find `add-face`.
///
/// The value is a `Vec<&WitFunction>` because multiple interfaces
/// CAN declare the same bare kebab name (rare in practice). Lookup
/// prefers interface-name hints when tie-breaking; absent any hint
/// it returns the first match deterministically (parse order is
/// stable file-sort order, so this is reproducible across regens).
pub fn index_wit_fns_nohyphen<'a>(
    fns: &'a [WitFunction],
) -> HashMap<String, Vec<&'a WitFunction>> {
    let mut idx: HashMap<String, Vec<&'a WitFunction>> = HashMap::new();
    for f in fns {
        // #547 (W3.1): same gate as `index_wit_fns` — methods aren't
        // free-function-callable so they don't belong here.
        // #556 (W3.1 mop-up): constructors ARE free-function-callable
        // from the SQL surface (`st_createtopology` etc.) so we keep
        // them in this index alongside the other free functions.
        if f.resource.is_some() && !f.is_constructor {
            continue;
        }
        let nh = f.kebab_name.replace('-', "");
        idx.entry(nh).or_default().push(f);
    }
    idx
}

/// Round-490: extended lookup. For each SQL-name candidate, try in
/// order:
///   1. exact snake match (today's behaviour)
///   2. no-underscore form in the no-hyphen index
///   3. with leading `st_` stripped, snake match
///   4. with leading `st_` stripped, no-underscore form in the
///      no-hyphen index
///
/// Catches `st_addface` → `add-face` (topology-edit),
/// `st_astopojson` → `as-topojson` (topology-output),
/// `st_aspng` → `as-png` (raster-output), etc.
pub fn find_wit_fn<'a>(
    candidates: &[String],
    snake_idx: &HashMap<String, &'a WitFunction>,
    nohyphen_idx: &HashMap<String, Vec<&'a WitFunction>>,
) -> Option<&'a WitFunction> {
    // Round-490 lookups, in order: exact snake → no-hyphen kebab →
    // `st_` prefix strip → `st_` strip with no-hyphen lookup. W1
    // adds a final pass that strips well-known "variant" suffixes
    // (mobilitydb's `_wit` / `_scalar` for the wit-value vs
    // primitive-binary surface duplicates) and retries the snake
    // match. The suffix strip is gated to the last fallback so the
    // exact match always wins for any SQL name that happens to end
    // in `_wit` etc. by coincidence.
    for cand in candidates {
        if let Some(f) = snake_idx.get(cand) {
            return Some(*f);
        }
        let cand_nh = cand.replace('_', "");
        if let Some(fs) = nohyphen_idx.get(&cand_nh) {
            if let Some(f) = pick_tiebreak(cand, fs) {
                return Some(f);
            }
        }
        if let Some(stripped) = cand.strip_prefix("st_") {
            if let Some(f) = snake_idx.get(stripped) {
                return Some(*f);
            }
            let stripped_nh = stripped.replace('_', "");
            if let Some(fs) = nohyphen_idx.get(&stripped_nh) {
                if let Some(f) = pick_tiebreak(cand, fs) {
                    return Some(f);
                }
            }
        }
    }
    // W1: suffix-strip fallback for mobilitydb's `_wit` / `_scalar`
    // duplicates. `tint_min_value_wit` and `tint_min_value_scalar`
    // are SQL surface variants of the same upstream WIT function
    // `tint-min-value` — the extension exposes both for
    // backwards-compat with two earlier dispatch paths.
    for cand in candidates {
        for suf in SCALAR_NAME_SUFFIXES {
            if let Some(bare) = cand.strip_suffix(suf) {
                if let Some(f) = snake_idx.get(bare) {
                    return Some(*f);
                }
            }
        }
    }
    None
}

/// Round-490 tie-break heuristic for the no-hyphen index. If the
/// candidate's stripped form starts with a known domain prefix
/// (e.g. `topo` → topology-* interfaces, `rast` → raster-*
/// interfaces) prefer matches in interfaces whose name contains
/// the same root. Otherwise return the first match (deterministic
/// because parse_dir walks files in sorted order).
pub fn pick_tiebreak<'a>(
    cand: &str,
    candidates: &[&'a WitFunction],
) -> Option<&'a WitFunction> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }
    // Domain-prefix preference. SQL names that look topology-ish
    // (`st_addface`, `st_createtopology`, `st_getfacegeometry`) get
    // routed to the postgis-topology-* interfaces; raster-ish names
    // (`st_aspng`, `st_arrayvalue`) get routed to postgis-raster-*.
    let domain_root = if cand.contains("topo")
        || cand.contains("face")
        || cand.contains("edge")
        || cand.contains("node")
    {
        Some("topology")
    } else if cand.contains("rast")
        || cand.contains("pixel")
        || cand.contains("band")
        || cand.contains("png")
        || cand.contains("tiff")
        || cand.contains("array")
    {
        Some("raster")
    } else {
        None
    };
    if let Some(root) = domain_root {
        for f in candidates {
            if f.interface.contains(root) {
                return Some(*f);
            }
        }
    }
    Some(candidates[0])
}

/// Phase F (#522): walk every `*.wit` under `wit_deps_dir` and
/// collect `type X = Y;` aliases from whichever package(s) are
/// declared. The dispatcher applies these to each `WitFunction`'s
/// params/ret before classify_* runs, so an alias like
/// `type timestamp-tz = s64;` resolves to `s64` and classifies
/// straight through to `OptionInt`/`Int` etc. instead of falling
/// through to `Unsupported("timestamp-tz")`.
pub fn collect_package_aliases(wit_deps_dir: &Path) -> Vec<WitTypeAlias> {
    match wit_parse::parse_package_dir(wit_deps_dir) {
        Ok(Some(pkg)) => pkg.type_aliases,
        _ => Vec::new(),
    }
}

/// W3.3 (#543): walk the primary shim's wit directory and collect
/// every `enum NAME { ... }` declaration. The dispatcher checks the
/// kebab name against this list (BEFORE the record-registry check)
/// so SQL scalars that take/return a WIT enum classify into
/// `ParamShape::Enum` / `RetShape::Enum` instead of falling through
/// to "param type not in dispatcher alphabet". Paralleled with
/// `collect_package_aliases` above.
pub fn collect_package_enums(wit_deps_dir: &Path) -> Vec<EnumWithPackage> {
    match wit_parse::parse_package_dir(wit_deps_dir) {
        Ok(Some(pkg)) => pkg
            .enums
            .into_iter()
            .map(|decl| EnumWithPackage {
                package: pkg.ns_name.clone(),
                decl,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Apply alias resolution to every parameter + return type in `fns`.
pub fn resolve_function_aliases(fns: Vec<WitFunction>, aliases: &[WitTypeAlias]) -> Vec<WitFunction> {
    if aliases.is_empty() {
        return fns;
    }
    fns.into_iter()
        .map(|f| WitFunction {
            params: f
                .params
                .into_iter()
                .map(|p| WitParam {
                    name: p.name,
                    ty: wit_parse::resolve_aliases(p.ty, aliases),
                })
                .collect(),
            ret: WitRet {
                inner: wit_parse::resolve_aliases(f.ret.inner, aliases),
                fallible: f.ret.fallible,
                // #565: preserve the error type, resolving aliases too
                // so a hand-aliased error ident (e.g. `type Err = postgis-
                // error;`) still surfaces as a named variant for the
                // reachability walker.
                error_ty: f
                    .ret
                    .error_ty
                    .map(|e| wit_parse::resolve_aliases(e, aliases)),
            },
            ..f
        })
        .collect()
}

/// #547 (W3.1): try each SQL-name candidate (and `st_`-stripped
/// variant) against the resource-method index. Returns the first
/// hit; the index is keyed by `<resource_snake>_<method_snake>`
/// so candidates like `topology_node_count` (from
/// `st_topologynodecount`'s alias list) match directly.
pub fn find_resource_method<'a>(
    candidates: &[String],
    method_idx: &HashMap<String, &'a WitFunction>,
) -> Option<&'a WitFunction> {
    for cand in candidates {
        if let Some(f) = method_idx.get(cand) {
            return Some(*f);
        }
        if let Some(bare) = cand.strip_prefix("st_") {
            if let Some(f) = method_idx.get(bare) {
                return Some(*f);
            }
        }
    }
    None
}

/// #556 (W3.1 mop-up): index every WIT resource → its DECLARING
/// interface. Built from any `WitFunction` whose `resource` field
/// is `Some(...)` (both methods and the constructor carry the
/// resource's owning interface in `f.interface`). The first
/// occurrence wins so a resource declared once stays bound to that
/// one interface even if `use`-imported in another.
pub fn index_resource_interfaces(fns: &[WitFunction]) -> HashMap<String, String> {
    let mut idx = HashMap::new();
    for f in fns {
        if let Some(ref rk) = f.resource {
            idx.entry(rk.clone()).or_insert_with(|| f.interface.clone());
        }
    }
    idx
}

/// #556 (W3.1 mop-up): same-interface name-matching fallback.
///
/// For each SQL-name candidate, split on each underscore (after
/// stripping any `st_` prefix). If the prefix is a known resource
/// kebab, look up its declaring interface and check whether THAT
/// interface declares a free function whose snake-name matches the
/// suffix.
///
/// Example: `st_topologyfrombytes` exposes the alias
/// `topology_from_bytes` in the interface DB; the SQL surface
/// candidate list includes both. After `st_` strip + first-`_`
/// split: prefix `topology`, suffix `from_bytes`. `topology` lives
/// in `postgis-topology-types`, which declares the free function
/// `from-bytes` (snake `from_bytes`). Match.
///
/// We try the snake-shaped form only — the no-hyphen variant is
/// already exercised by `find_wit_fn` upstream so the gap this
/// fallback fills is specifically the `<resource>_<func>` shape
/// (underscored aliases like `topology_from_bytes`). Returning the
/// first interface-restricted hit keeps behaviour deterministic
/// (parse order is stable).
pub fn find_same_interface_free_fn<'a>(
    candidates: &[String],
    snake_idx: &HashMap<String, &'a WitFunction>,
    resource_iface_idx: &HashMap<String, String>,
) -> Option<&'a WitFunction> {
    for cand in candidates {
        let raw = cand.strip_prefix("st_").unwrap_or(cand);
        let bytes = raw.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b != b'_' {
                continue;
            }
            let prefix = &raw[..i];
            let suffix = &raw[i + 1..];
            if prefix.is_empty() || suffix.is_empty() {
                continue;
            }
            // Resource kebab names are typically single tokens
            // (`topology`, `raster`); convert any embedded `_` back
            // to `-` defensively for multi-token resources.
            let resource_kebab = prefix.replace('_', "-");
            let Some(iface) = resource_iface_idx.get(&resource_kebab) else {
                continue;
            };
            if let Some(f) = snake_idx.get(suffix) {
                if f.interface == *iface {
                    return Some(*f);
                }
            }
        }
    }
    None
}

/// #673: no-hyphen resource-method index for concatenated SQL
/// names. Keyed by `<resource_nohyphen><method_nohyphen>` so a
/// SQL scalar like `st_topologynodecount` (no separator at all)
/// resolves directly after stripping `st_`.
///
/// The interface DB ships a handful of postgis topology aliases
/// in this concatenated form (`st_topologyedgecount`,
/// `st_topologyfacecount`, `st_topologyname`,
/// `st_topologyprecision`, `st_topologysrid`,
/// `st_topologytobytes`, `st_topologynodecount`). Their
/// underscored siblings (`st_topology_node_count`) resolve via
/// `index_resource_methods`; this index covers the gap where
/// the SQL surface omits the separator entirely.
pub fn index_resource_methods_concat<'a>(
    fns: &'a [WitFunction],
) -> HashMap<String, &'a WitFunction> {
    let mut idx = HashMap::new();
    for f in fns {
        let Some(ref rkebab) = f.resource else {
            continue;
        };
        if f.is_constructor {
            continue;
        }
        let key = format!(
            "{}{}",
            rkebab.replace('-', ""),
            f.kebab_name.replace('-', ""),
        );
        idx.insert(key, f);
    }
    idx
}

/// #673: concatenated-form resource match. Catches SQL names that
/// glue `<resource><verb>` with NO separator at all
/// (`st_topologynodecount`, `st_topologyfrombytes`), which the
/// earlier `find_resource_method` / `find_same_interface_free_fn`
/// / `find_resource_family_free_fn` passes can't split because
/// they look for an underscore between the two halves.
///
/// Walk per candidate (after `st_` strip):
///   1. Direct lookup in the concat method index
///      (`topologynodecount` → topology::node-count).
///   2. For each known resource, peel its no-hyphen kebab off the
///      front and look the remainder up in the no-hyphen free-fn
///      index, gated to interfaces in the resource's
///      `<ns>-<resource>-*` family. Covers free fns colocated
///      with the resource type (`st_topologyfrombytes` →
///      `postgis-topology-types::from-bytes`).
///
/// Candidates that already contain an underscore (after `st_`
/// strip) are skipped — those have a separator to split on and
/// resolve through the prior passes.
pub fn find_resource_concat_match<'a>(
    candidates: &[String],
    method_concat_idx: &HashMap<String, &'a WitFunction>,
    nohyphen_idx: &HashMap<String, Vec<&'a WitFunction>>,
    resource_iface_idx: &HashMap<String, String>,
) -> Option<&'a WitFunction> {
    for cand in candidates {
        let raw = cand.strip_prefix("st_").unwrap_or(cand);
        if raw.contains('_') {
            continue;
        }
        // 1) Direct resource-method concat key.
        if let Some(f) = method_concat_idx.get(raw) {
            return Some(*f);
        }
        // 2) Resource-prefix peel + free-fn family lookup.
        for (resource_kebab, declaring_iface) in resource_iface_idx {
            let resource_nh = resource_kebab.replace('-', "");
            let Some(remainder) = raw.strip_prefix(&resource_nh) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }
            let Some(last_dash) = declaring_iface.rfind('-') else {
                continue;
            };
            let family_prefix = &declaring_iface[..=last_dash];
            if let Some(matches) = nohyphen_idx.get(remainder) {
                for f in matches {
                    if f.interface.starts_with(family_prefix) {
                        return Some(*f);
                    }
                }
            }
        }
    }
    None
}

/// Round (#672): broader sibling of `find_same_interface_free_fn`.
///
/// `find_same_interface_free_fn` only matches a free function in
/// the interface that DECLARES the resource (e.g. `topology` lives
/// in `postgis-topology-types`, so `topology_from_bytes` resolves
/// to `postgis-topology-types::from-bytes`). But sibling interfaces
/// in the same `<ns>-<resource>-*` family — `postgis-topology-edit`,
/// `postgis-topology-output`, `postgis-topology-query`,
/// `postgis-topology-topogeom`, plus `postgis-raster-*` for the
/// raster family — also declare free functions that take a
/// `borrow<resource>` first parameter and the interface DB advertises
/// them as `topology_<func>` / `raster_<func>` SQL scalars. Those
/// miss the declaring-interface gate.
///
/// This helper relaxes the interface match from exact-equality to
/// `<ns>-<resource>-*` prefix-membership. For SQL `topology_mod_edge_heal`:
///   1. Split at first `_` (after `st_` strip): prefix=`topology`,
///      suffix=`mod_edge_heal`.
///   2. Resource `topology` declared in `postgis-topology-types` →
///      family prefix `postgis-topology-`.
///   3. Look up `mod_edge_heal` in the snake index → returns
///      `postgis-topology-edit::mod-edge-heal`. Interface starts with
///      `postgis-topology-`? Yes → match.
///
/// Two extra forms are tried beyond the bare suffix to handle WIT
/// kebabs that don't follow the verb-after-resource convention:
///   - `<suffix>_<prefix>` — covers `topology_validate` →
///     `validate-topology` (kebab puts resource AFTER the verb).
///   - `<prefix>_<suffix>` — covers `topology_create` →
///     `create-topology` (resource constructor; kebab is
///     `create-<resource>` per #556 W3.1).
///
/// Returns the first interface-family-restricted hit so behaviour
/// stays deterministic (parse order is stable file-sort order).
pub fn find_resource_family_free_fn<'a>(
    candidates: &[String],
    snake_idx: &HashMap<String, &'a WitFunction>,
    resource_iface_idx: &HashMap<String, String>,
) -> Option<&'a WitFunction> {
    for cand in candidates {
        let raw = cand.strip_prefix("st_").unwrap_or(cand);
        let bytes = raw.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b != b'_' {
                continue;
            }
            let prefix = &raw[..i];
            let suffix = &raw[i + 1..];
            if prefix.is_empty() || suffix.is_empty() {
                continue;
            }
            let resource_kebab = prefix.replace('_', "-");
            let Some(declaring_iface) = resource_iface_idx.get(&resource_kebab) else {
                continue;
            };
            // Family prefix = declaring interface name up to and
            // including the resource segment. `postgis-topology-types`
            // → drop the trailing `-types` (the last `-<segment>`)
            // → family prefix `postgis-topology-`. Sibling interfaces
            // (`postgis-topology-edit`, etc.) start with that prefix.
            let Some(last_dash) = declaring_iface.rfind('-') else {
                continue;
            };
            let family_prefix = &declaring_iface[..=last_dash];
            // Three lookups, in order: bare suffix (verb-only), then
            // suffix-prefix swap (verb-after-resource kebab shape),
            // then prefix-suffix join (constructor `create-<resource>`).
            let bare = suffix.to_string();
            let swapped = format!("{}_{}", suffix, prefix);
            let joined = format!("{}_{}", prefix, suffix);
            for k in [&bare, &swapped, &joined] {
                if let Some(f) = snake_idx.get(k) {
                    if f.interface.starts_with(family_prefix) {
                        return Some(*f);
                    }
                }
            }
        }
    }
    None
}
