//! Lightweight WIT parser for Phase 3 dispatch-registry generation.
//!
//! The codegen's wasm-component target needs to know, for each
//! scalar/aggregate the interface DB declares, WHICH WIT interface
//! hosts the matching function and WHAT its parameter / return
//! signature looks like. The interface DB only carries the
//! upstream-extension name + a list of SQL-side parameter type
//! strings; the WIT carries the actual import paths and the
//! exact Rust-binding shapes.
//!
//! Rather than depend on the full `wit-parser` crate (heavy + ties
//! the codegen to a specific wasm-tools version), Phase 3 ships a
//! small purpose-built parser. The postgis-wasm WIT files follow
//! a tight, regular format: every function declaration is one
//! `kebab-name: func(args) -> return-type;` line per file. The
//! parser scans for `interface NAME {` blocks and the
//! `kebab-name: func(...)` lines inside them; it ignores
//! everything else (`use ...`, `record ...`, `enum ...`,
//! `type ...`, doc comments, blank lines, block comments).
//!
//! Output is a `Vec<WitFunction>` — every function the codegen
//! can possibly route to. Pairing with the interface DB happens
//! one layer up in `dispatch.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One WIT-side function the codegen may want to dispatch to.
#[derive(Debug, Clone)]
pub struct WitFunction {
    /// Owning WIT package, e.g. `postgis:wasm` or `mobilitydb:temporal`.
    /// Used to emit the right `use bindings::<ns>::<name>::<module>`
    /// path in the generated lib.rs. Phase D extension.
    pub package: String,
    /// Owning WIT package version, e.g. `0.1.0`. Used by callers
    /// that need to emit symbolic names including the version.
    pub package_version: String,
    /// Bare WIT interface name (without the package prefix), e.g.
    /// `postgis-constructors`. Used by `dispatch.rs` to compute
    /// the Rust binding module alias (`pg_ctor`, etc.).
    pub interface: String,
    /// Kebab-case function name, e.g. `st-geom-from-text`. Matches
    /// the `wit_func` field of the dispatch arms emitted by
    /// `dispatch::emit_arm_body`.
    pub kebab_name: String,
    /// Parameter list in source order.
    pub params: Vec<WitParam>,
    /// Return-type shape.
    pub ret: WitRet,
    /// #547 (W3.1): when `Some(resource_kebab)`, this WitFunction is
    /// a METHOD on the named resource (declared inside a
    /// `resource NAME { name: func(...) -> ...; }` block). The
    /// call expression is `arg{0}.{method_snake}({args})` rather
    /// than `{module}::{func}({args})` — the first param is the
    /// resource receiver (decoded from blob via the resource's
    /// `from-*` free function). When `None`, this is a free
    /// function in `interface`.
    pub resource: Option<String>,
    /// #556 (W3.1 mop-up): when `true`, this WitFunction is the
    /// CONSTRUCTOR of `resource` (a `constructor(args)` line inside
    /// a `resource NAME { ... }` block). Constructors look method-
    /// like in the WIT but their call form is `<Pascal>::new(args)`
    /// and their return is the resource itself (no receiver in the
    /// arg list). The kebab name is synthesised as
    /// `create-<resource_kebab>` (e.g. `create-topology`) so the
    /// SQL-name lookup paths (`st_createtopology`,
    /// `st_create_topology`, `topology_create`) all resolve to it
    /// via the existing snake / no-hyphen indexes.
    pub is_constructor: bool,
}

/// Lightweight summary of one WIT package, used by `emit_wit` to
/// build the `world.wit` import list dynamically per shim. Phase D.
#[derive(Debug, Clone)]
pub struct WitPackage {
    /// Package namespace + name (e.g. `postgis:wasm`,
    /// `sqlite:extension`, `mobilitydb:temporal`).
    pub ns_name: String,
    /// Package version (e.g. `0.1.0`).
    pub version: String,
    /// Every `interface NAME` block declared inside the package.
    pub interfaces: Vec<String>,
    /// Records declared in the package, keyed by owning interface.
    /// Field order matches the WIT source. Phase C extension.
    pub records: Vec<WitRecord>,
    /// Resources declared in the package (e.g. `resource geometry`),
    /// keyed by owning interface. Phase D D3/D4 use this to gate
    /// emission of postgis-specific helpers.
    pub resources: Vec<WitResourceDecl>,
    /// Variant declarations (e.g. `variant postgis-error`), keyed
    /// by owning interface. Phase D D3/D4 use this to gate
    /// emission of postgis-specific error helpers.
    pub variants: Vec<WitVariantDecl>,
    /// Enum declarations (e.g. `enum interpolation`). Phase E uses
    /// this to enumerate types wit-bindgen will derive on so we can
    /// pass them in `additional_derives_ignore` when they aren't
    /// part of the primary shim's serde-ops surface.
    pub enums: Vec<WitEnumDecl>,
    /// Flags declarations (e.g. `flags function-flags`). Same
    /// purpose as `enums` — Phase E uses these to skip
    /// `additional_derives` on contract bitflags types that don't
    /// derive Serialize out-of-box.
    pub flags: Vec<WitFlagsDecl>,
    /// `type X = Y;` aliases. Phase E inline-substitutes these
    /// inside local serde-ops records.
    pub type_aliases: Vec<WitTypeAlias>,
}

/// One `record NAME { field: type, ... }` declaration parsed from
/// a WIT source. Phase C records walker.
#[derive(Debug, Clone)]
pub struct WitRecord {
    pub interface: String,
    /// kebab-case record name as written in the WIT
    /// (e.g. `tfloat-sequence`).
    pub kebab_name: String,
    /// Ordered list of fields as `(kebab_field_name, raw_type_text)`.
    /// The raw type text is kept verbatim so canonical hashing
    /// matches the source layout exactly.
    pub fields: Vec<(String, String)>,
}

/// One `resource NAME` declaration parsed from a WIT source.
#[derive(Debug, Clone)]
pub struct WitResourceDecl {
    pub interface: String,
    pub kebab_name: String,
}

/// One `variant NAME { ... }` declaration parsed from a WIT source.
#[derive(Debug, Clone)]
pub struct WitVariantDecl {
    pub interface: String,
    pub kebab_name: String,
}

/// One `enum NAME { ... }` declaration parsed from a WIT source.
#[derive(Debug, Clone)]
pub struct WitEnumDecl {
    pub interface: String,
    pub kebab_name: String,
    /// Kebab-case case names, in declaration order. Used by
    /// `render_local_enum` when emitting a copy of this enum
    /// inside the bridge's local serde-ops interface.
    pub cases: Vec<String>,
}

/// One `flags NAME { ... }` declaration parsed from a WIT source.
#[derive(Debug, Clone)]
pub struct WitFlagsDecl {
    pub interface: String,
    pub kebab_name: String,
}

/// One `type X = Y;` declaration parsed from a WIT source. Phase E
/// uses these to inline-substitute aliased type names inside the
/// bridge's local serde-ops records (so a field declared `timestamp:
/// timestamp-tz` becomes `timestamp: s64` in the local copy without
/// needing to also duplicate the alias).
#[derive(Debug, Clone)]
pub struct WitTypeAlias {
    pub interface: String,
    pub kebab_name: String,
    /// RHS of the `type X = Y;` line, with no trailing semicolon.
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct WitParam {
    /// Parameter name as written in the WIT.
    pub name: String,
    /// Normalised type shape.
    pub ty: WitType,
}

/// Limited type alphabet recognised by Phase 3.
///
/// The shape is intentionally narrow — Phase 3 wires the simple
/// scalar shapes that account for the vast bulk of the PostGIS
/// surface. Functions whose WIT signature mixes types the
/// dispatcher can't translate (option<...>, lists, tuples,
/// records other than `geometry` / `geography` / `raster` /
/// `topology` / `topo-geometry`) get noted by `dispatch.rs` and
/// left as stub arms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitType {
    /// `borrow<geometry>` or `geometry` (owned).
    Geometry { borrowed: bool },
    /// `borrow<geography>` or `geography` (owned).
    Geography { borrowed: bool },
    /// `borrow<raster>` or `raster` (owned). Round-490 raster shim
    /// support: PostGIS raster resource. Decoded from
    /// `SqlValue::Blob` via `postgis-raster-types::from-binary`;
    /// encoded by calling the resource's `.as_binary()` method.
    Raster { borrowed: bool },
    /// `borrow<topology>` or `topology` (owned). Round-490 topology
    /// shim support: PostGIS topology resource. Decoded from
    /// `SqlValue::Blob` via `postgis-topology-types::from-bytes`;
    /// encoded by calling the resource's `.to_bytes()` method.
    Topology { borrowed: bool },
    /// `string`
    String,
    /// `f64`
    F64,
    /// `f32`
    F32,
    /// `s32`
    S32,
    /// `s64`
    S64,
    /// `u32`
    U32,
    /// `u64`
    U64,
    /// `u8`
    U8,
    /// `bool`
    Bool,
    /// `list<u8>` — raw bytes (WKB, EWKB, etc.).
    ListU8,
    /// `list<borrow<geometry>>` — variadic geometry input. The SQL
    /// surface flattens this into N separate BLOB args at the
    /// dispatcher; each arg is decoded to a `Geometry` and the
    /// borrowed-ref slice is built from the resulting owned vec.
    /// Round 2 extension.
    ListGeomBorrow,
    /// `list<borrow<raster>>` — raster equivalent of `ListGeomBorrow`.
    /// Today only surfaces in aggregate input (`st-rast-union-aggregate`);
    /// the aggregate accumulator decodes each blob via
    /// `Raster::from_binary` at finalize. #548 (W3.2) extension.
    ListRasterBorrow,
    /// `list<geometry>` — owned geometry result list. Round 2
    /// extension for cluster aggregates whose WIT signature
    /// returns `list<geometry>` (one collection per cluster).
    ListGeomOwned,
    /// `list<option<u32>>` — owned list of optional cluster ids
    /// (DBSCAN's "None == noise" convention). Round 2 extension
    /// used for `st_clusterdbscan` aggregate return shape.
    ListOptionU32,
    /// `option<T>` — the call site passes `None` for these in
    /// Phase 3, which mirrors the SQL-side convention of "use
    /// the function's default" when the argument is omitted.
    Option(Box<WitType>),
    /// `tuple<T1, T2, ...>` — heterogeneous N-tuple. Round 3
    /// extension. Recognised so that:
    /// - `option<tuple<...>>` params can be classified (and
    ///   dispatched as `None`) without falling through to
    ///   `Unsupported`. `st-tile-envelope`'s `bounds` arg uses
    ///   this shape.
    /// - The specific return-shape
    ///   `tuple<bool, option<string>, option<geometry>>` from
    ///   `st-is-valid-detail` can be marshaled to text.
    Tuple(Vec<WitType>),
    /// `bbox` record from postgis-types — `{min-x, min-y, max-x,
    /// max-y}` of 4 f64s. Round 3 extension. Returned by
    /// `st-make-box2d` and `st-box-from-geohash`; the dispatcher
    /// projects to a WKB POLYGON envelope so the interface DB's
    /// `binary` return type is honoured.
    Bbox,
    /// Generic `list<T>` over any non-specialized element type.
    /// Phase F (#522). The specialized variants above (`ListU8`,
    /// `ListGeomBorrow`, `ListGeomOwned`, `ListOptionU32`) take
    /// precedence in `parse_type`; everything else (e.g.
    /// `list<s64>`, `list<tfloat-instant>`, `list<int-span>`)
    /// surfaces here so the dispatcher can choose to wire it.
    List(Box<WitType>),
    /// Generic `result<T, E>` nested inside another compound.
    /// `parse_return_body` still strips top-level `result<...>` into
    /// the `WitRet.fallible` flag, but a nested `option<result<T,
    /// E>>` (or similar) surfaces here so the dispatcher sees the
    /// shape and can fail cleanly with a named error if it can't
    /// wire it. Phase F (#522).
    Result(Box<WitType>, Box<WitType>),
    /// Anything we don't know how to marshal yet. Captures the
    /// original text so the diagnostic in `dispatch.rs` can name
    /// it concretely.
    Unsupported(String),
}

/// Return-type shape. Either bare T or `result<T, postgis-error>`.
#[derive(Debug, Clone)]
pub struct WitRet {
    pub inner: WitType,
    /// True iff the source signature was `result<T, postgis-error>`.
    pub fallible: bool,
    /// #565 (#557fix.2): when `fallible`, the parsed error type from
    /// the source `result<T, E>` clause. Pre-#565 the parser stripped
    /// `result<...>` and threw the error half away; the reachability
    /// walker for the force-link block needs to see the error variant
    /// referenced (otherwise `postgis-error` looks unreached and gets
    /// DCE'd out of the emit list even though wit-bindgen keeps it).
    /// `None` for non-fallible returns and for malformed result types
    /// that don't carry a comma-separated error type.
    pub error_ty: Option<WitType>,
}

/// Parse every `.wit` file under `dir`, returning every function
/// the parser recognises. Files that aren't part of a
/// `package postgis:wasm@...;` declaration are still scanned —
/// the postgis-wasm/ deps directory contains only postgis-wasm
/// files in practice.
pub fn parse_dir(dir: &Path) -> Result<Vec<WitFunction>> {
    let mut out = Vec::new();
    for_each_wit_file(dir, |text| parse_text(text, &mut out))?;
    Ok(out)
}

/// Parse one WIT package directory — a single dir containing one
/// or more `*.wit` files that all declare the SAME package. Phase D.
///
/// Returns `None` when no package declaration is found in any file
/// (e.g. the dir contains only doc fragments).
pub fn parse_package_dir(dir: &Path) -> Result<Option<WitPackage>> {
    let mut ns_name = None::<String>;
    let mut version = None::<String>;
    let mut interfaces = Vec::new();
    let mut records = Vec::new();
    let mut resources = Vec::new();
    let mut variants = Vec::new();
    let mut enums = Vec::new();
    let mut flags = Vec::new();
    let mut type_aliases = Vec::new();
    let mut seen_interface = std::collections::BTreeSet::new();
    for_each_wit_file(dir, |text| {
        if let Some((n, v)) = parse_package_decl(text) {
            // Multiple files may repeat the same package; OK.
            ns_name.get_or_insert(n);
            version.get_or_insert(v);
        }
        let mut decls = scan_package_decls(text);
        for ifname in decls.interfaces.drain(..) {
            if seen_interface.insert(ifname.clone()) {
                interfaces.push(ifname);
            }
        }
        records.append(&mut decls.records);
        resources.append(&mut decls.resources);
        variants.append(&mut decls.variants);
        enums.append(&mut decls.enums);
        flags.append(&mut decls.flags);
        type_aliases.append(&mut decls.type_aliases);
    })?;
    let (ns_name, version) = match (ns_name, version) {
        (Some(n), Some(v)) => (n, v),
        _ => return Ok(None),
    };
    Ok(Some(WitPackage {
        ns_name,
        version,
        interfaces,
        records,
        resources,
        variants,
        enums,
        flags,
        type_aliases,
    }))
}

fn for_each_wit_file<F>(dir: &Path, mut f: F) -> Result<()>
where
    F: FnMut(&str),
{
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading wit dir {}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "wit")
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    for path in files {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        f(&text);
    }
    Ok(())
}

/// Returns `Some((ns_name, version))` from a `package NS:NAME@VER;`
/// line, scanning only the first non-comment line.
fn parse_package_decl(text: &str) -> Option<(String, String)> {
    let stripped = strip_block_comments(text);
    for raw in stripped.lines() {
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package ") {
            // rest: `ns:name@version;` (possibly followed by other things)
            let rest = rest.trim_end_matches(';').trim();
            let at = rest.find('@')?;
            let ns_name = rest[..at].trim().to_string();
            let ver = rest[at + 1..].trim().trim_end_matches(';').to_string();
            if ns_name.is_empty() || ver.is_empty() {
                return None;
            }
            return Some((ns_name, ver));
        }
    }
    None
}

struct PackageDecls {
    interfaces: Vec<String>,
    records: Vec<WitRecord>,
    resources: Vec<WitResourceDecl>,
    variants: Vec<WitVariantDecl>,
    enums: Vec<WitEnumDecl>,
    flags: Vec<WitFlagsDecl>,
    type_aliases: Vec<WitTypeAlias>,
}

/// Walk a WIT source and pull out every `interface NAME {` opener,
/// every `record NAME { ... }` block, every `resource NAME` line,
/// and every `variant NAME { ... }` block. Used by `parse_package_dir`.
///
/// The parser is line-oriented and tracks brace depth so records
/// nested in an interface are correctly attributed.
fn scan_package_decls(text: &str) -> PackageDecls {
    let stripped = strip_block_comments(text);
    let mut interfaces = Vec::new();
    let mut records = Vec::new();
    let mut resources = Vec::new();
    let mut variants = Vec::new();
    let mut enums = Vec::new();
    let mut flags = Vec::new();
    let mut type_aliases = Vec::new();

    let mut cur_interface: Option<String> = None;
    let mut depth_in_iface: i32 = 0;

    let mut pending_record: Option<(String, Vec<(String, String)>)> = None;
    let mut record_depth: i32 = 0;

    let mut in_variant: Option<String> = None;
    let mut variant_depth: i32 = 0;
    // `pending_enum` mirrors `pending_record`: we accumulate the
    // enum's case list until the closing brace, then publish.
    // `pending_flags` similarly tracks the body so the parser
    // doesn't get confused on nested braces (none for now, but
    // future grammars may add them).
    let mut pending_enum: Option<(String, Vec<String>)> = None;
    let mut enum_depth: i32 = 0;
    let mut in_flags: Option<String> = None;
    let mut flags_depth: i32 = 0;

    for raw in stripped.lines() {
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;

        // Inside a record block — collect its fields.
        if let Some((iface_name, fields)) = pending_record.as_mut() {
            // Adjust record-depth tracking first.
            record_depth += opens - closes;
            // Fields look like `field-name: type-expr,` (or without trailing comma on last).
            // Parse only when we're at depth 1 (the open brace was counted).
            // Skip the line that closes the record.
            if record_depth <= 0 {
                // Flush the record.
                let rec = WitRecord {
                    interface: iface_name.clone(),
                    kebab_name: fields[0].0.clone(),
                    fields: fields[1..].to_vec(),
                };
                let _ = rec; // placeholder; constructed below
                // Reconstruct correctly: kebab_name was stashed in fields[0].0
                let kebab = fields.remove(0).0;
                let rec = WitRecord {
                    interface: iface_name.clone(),
                    kebab_name: kebab,
                    fields: std::mem::take(fields),
                };
                records.push(rec);
                pending_record = None;
                record_depth = 0;
                // Adjust outer interface depth.
                if cur_interface.is_some() {
                    depth_in_iface += opens - closes;
                    if depth_in_iface <= 0 {
                        cur_interface = None;
                        depth_in_iface = 0;
                    }
                }
                continue;
            }
            // Otherwise, this line might be one or more field
            // declarations. Some upstream WIT files squash multiple
            // primitive fields onto a single line, e.g.
            //   `xmin: f64, ymin: f64, xmax: f64, ymax: f64,`
            // Split on top-level commas (depth-0 wrt angle brackets
            // so that generic types like `list<u8>` and `tuple<f64,
            // f64>` aren't shredded), then parse each segment as a
            // `field: type` pair. Task #523: this matters because
            // the structural-identity check needs the parsed
            // type-text to be a recognised primitive/ident in order
            // to flag the record as `direct`.
            let body = line.trim_end_matches(',').trim();
            for seg in split_top_level_comma(body) {
                let seg = seg.trim();
                if seg.is_empty() {
                    continue;
                }
                if let Some(colon) = seg.find(':') {
                    let fname = seg[..colon].trim();
                    let ftype = seg[colon + 1..].trim();
                    if !fname.is_empty() && is_kebab_ident(fname) {
                        fields.push((fname.to_string(), ftype.to_string()));
                    }
                }
            }
            continue;
        }

        // Inside a variant block — we don't need field details, just track depth.
        if let Some(_vname) = in_variant.as_ref() {
            variant_depth += opens - closes;
            if variant_depth <= 0 {
                in_variant = None;
                variant_depth = 0;
                if cur_interface.is_some() {
                    depth_in_iface += opens - closes;
                    if depth_in_iface <= 0 {
                        cur_interface = None;
                        depth_in_iface = 0;
                    }
                }
            }
            continue;
        }

        // Inside a flags block — track depth only.
        if let Some(_fname) = in_flags.as_ref() {
            flags_depth += opens - closes;
            if flags_depth <= 0 {
                in_flags = None;
                flags_depth = 0;
                if cur_interface.is_some() {
                    depth_in_iface += opens - closes;
                    if depth_in_iface <= 0 {
                        cur_interface = None;
                        depth_in_iface = 0;
                    }
                }
            }
            continue;
        }

        // Inside an enum block — collect each case name. WIT enum
        // syntax is `enum NAME { case1, case2, ... }`, one identifier
        // per case, comma-separated. A case name is a kebab ident.
        if let Some((ename, cases)) = pending_enum.as_mut() {
            enum_depth += opens - closes;
            if enum_depth <= 0 {
                // Flush.
                let iface_name = cur_interface.clone().unwrap_or_default();
                let kebab = ename.clone();
                let cases_taken = std::mem::take(cases);
                enums.push(WitEnumDecl {
                    interface: iface_name.clone(),
                    kebab_name: kebab,
                    cases: cases_taken,
                });
                pending_enum = None;
                enum_depth = 0;
                if cur_interface.is_some() {
                    depth_in_iface += opens - closes;
                    if depth_in_iface <= 0 {
                        cur_interface = None;
                        depth_in_iface = 0;
                    }
                }
                continue;
            }
            // Pull each kebab ident out of the line's content. Cases
            // may be comma-separated; tolerate trailing commas.
            let body = line.trim_end_matches(',').trim();
            // Skip the opening brace line where the body is just `{`.
            for piece in body.split(',') {
                let s = piece.trim();
                if s.is_empty() {
                    continue;
                }
                if is_kebab_ident(s) {
                    cases.push(s.to_string());
                }
            }
            continue;
        }

        if cur_interface.is_none() {
            if let Some(name) = parse_interface_open(line) {
                cur_interface = Some(name.clone());
                interfaces.push(name);
                depth_in_iface = 1;
                continue;
            }
        } else {
            // Try to detect record / variant / resource at the top of the interface.
            let iface_name = cur_interface.clone().unwrap();
            if let Some(rname) = parse_record_open(line) {
                // Start collecting fields. We stash the record name as the
                // first sentinel "field" with empty type and pop later.
                pending_record =
                    Some((iface_name.clone(), vec![(rname.clone(), String::new())]));
                // Adjust depth: the `{` of the record counts inside the interface,
                // but we track it via record_depth separately.
                record_depth = opens - closes;
                if record_depth <= 0 {
                    // single-line record. Inline fields may be on
                    // the same line: `record N { a: f64, b: f64 }`.
                    // Parse them out of the brace body.
                    let mut inline_fields: Vec<(String, String)> = Vec::new();
                    if let Some(open_idx) = line.find('{') {
                        if let Some(close_idx) = line.rfind('}') {
                            if open_idx < close_idx {
                                let body = &line[open_idx + 1..close_idx];
                                for piece in split_field_pieces(body) {
                                    let s = piece.trim();
                                    if s.is_empty() {
                                        continue;
                                    }
                                    if let Some(colon) = s.find(':') {
                                        let fname = s[..colon].trim();
                                        let ftype = s[colon + 1..].trim();
                                        if !fname.is_empty() && is_kebab_ident(fname) {
                                            inline_fields.push((
                                                fname.to_string(),
                                                ftype.to_string(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    records.push(WitRecord {
                        interface: iface_name.clone(),
                        kebab_name: rname,
                        fields: inline_fields,
                    });
                    pending_record = None;
                    record_depth = 0;
                }
                // The interface itself sees the same `{`/`}`; account for that.
                depth_in_iface += opens - closes;
                if depth_in_iface <= 0 {
                    cur_interface = None;
                    depth_in_iface = 0;
                }
                continue;
            }
            if let Some(vname) = parse_variant_open(line) {
                variants.push(WitVariantDecl {
                    interface: iface_name.clone(),
                    kebab_name: vname.clone(),
                });
                in_variant = Some(vname);
                variant_depth = opens - closes;
                if variant_depth <= 0 {
                    in_variant = None;
                    variant_depth = 0;
                }
                depth_in_iface += opens - closes;
                if depth_in_iface <= 0 {
                    cur_interface = None;
                    depth_in_iface = 0;
                }
                continue;
            }
            if let Some(rname) = parse_resource_decl(line) {
                resources.push(WitResourceDecl {
                    interface: iface_name.clone(),
                    kebab_name: rname,
                });
            }
            if let Some((tname, body)) = parse_type_alias(line) {
                type_aliases.push(WitTypeAlias {
                    interface: iface_name.clone(),
                    kebab_name: tname,
                    body,
                });
            }
            if let Some(ename) = parse_enum_open(line) {
                // Stash the enum; we'll collect cases as the body
                // unfolds and flush in the `pending_enum`
                // dispatch above. Single-line empty enums are
                // flushed immediately.
                let mut initial_cases: Vec<String> = Vec::new();
                // Parse any inline cases from the same line — WIT
                // permits `enum NAME { case1, case2 }` on one
                // line.
                if let Some(brace_start) = line.find('{') {
                    let after = &line[brace_start + 1..];
                    let body = after.trim_end_matches('}').trim_end_matches(',');
                    for piece in body.split(',') {
                        let s = piece.trim();
                        if s.is_empty() {
                            continue;
                        }
                        if is_kebab_ident(s) {
                            initial_cases.push(s.to_string());
                        }
                    }
                }
                pending_enum = Some((ename.clone(), initial_cases));
                enum_depth = opens - closes;
                if enum_depth <= 0 {
                    // Single-line empty / inline enum — flush
                    // immediately. The `pending_enum` dispatch above
                    // won't run on the next line because we set
                    // pending_enum = None here.
                    if let Some((kebab, cases)) = pending_enum.take() {
                        enums.push(WitEnumDecl {
                            interface: iface_name.clone(),
                            kebab_name: kebab,
                            cases,
                        });
                    }
                    enum_depth = 0;
                }
                depth_in_iface += opens - closes;
                if depth_in_iface <= 0 {
                    cur_interface = None;
                    depth_in_iface = 0;
                }
                continue;
            }
            if let Some(fname) = parse_flags_open(line) {
                flags.push(WitFlagsDecl {
                    interface: iface_name.clone(),
                    kebab_name: fname.clone(),
                });
                in_flags = Some(fname);
                flags_depth = opens - closes;
                if flags_depth <= 0 {
                    in_flags = None;
                    flags_depth = 0;
                }
                depth_in_iface += opens - closes;
                if depth_in_iface <= 0 {
                    cur_interface = None;
                    depth_in_iface = 0;
                }
                continue;
            }
            depth_in_iface += opens - closes;
            if depth_in_iface <= 0 {
                cur_interface = None;
                depth_in_iface = 0;
            }
        }
    }

    PackageDecls {
        interfaces,
        records,
        resources,
        variants,
        enums,
        flags,
        type_aliases,
    }
}

/// Returns `Some((alias_name, alias_body))` from a `type X = Y;` line.
/// The body is the RHS with trailing semicolon stripped. Used by
/// Phase E to inline-substitute aliases inside local serde-ops
/// records.
fn parse_type_alias(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("type ")?;
    let eq = rest.find('=')?;
    let name = rest[..eq].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    let body = rest[eq + 1..].trim().trim_end_matches(';').trim().to_string();
    if body.is_empty() {
        return None;
    }
    Some((name, body))
}

/// Returns `Some(enum_name)` when `line` opens an `enum NAME {` block.
fn parse_enum_open(line: &str) -> Option<String> {
    let rest = line.strip_prefix("enum ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    Some(name)
}

/// Returns `Some(flags_name)` when `line` opens a `flags NAME {` block.
fn parse_flags_open(line: &str) -> Option<String> {
    let rest = line.strip_prefix("flags ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    Some(name)
}

/// Split a record-body string at top-level commas (commas not
/// inside `<...>` or `(...)`). Used by the single-line record
/// inline-field parse path so `list<u8>` or `option<f64>` don't
/// trip the splitter.
fn split_field_pieces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start: usize = 0;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' => depth += 1,
            b'>' | b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        out.push(s[start..].to_string());
    }
    out
}

/// Returns `Some(record_name)` when `line` opens a `record NAME {`.
fn parse_record_open(line: &str) -> Option<String> {
    let rest = line.strip_prefix("record ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    Some(name)
}

/// Returns `Some(variant_name)` when `line` opens a `variant NAME {`.
fn parse_variant_open(line: &str) -> Option<String> {
    let rest = line.strip_prefix("variant ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    Some(name)
}

/// Returns `Some(resource_name)` when `line` is `resource NAME;` or
/// `resource NAME { ... }` (we treat the body as opaque).
fn parse_resource_decl(line: &str) -> Option<String> {
    let rest = line.strip_prefix("resource ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{' || c == ';')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() || !is_kebab_ident(&name) {
        return None;
    }
    Some(name)
}

/// Parse one WIT source's text into the running collection.
fn parse_text(text: &str, out: &mut Vec<WitFunction>) {
    // Strip block comments `/* ... */` up front so they don't
    // confuse the brace counter; line comments and doc comments
    // are handled per-line below.
    let stripped = strip_block_comments(text);

    // Phase D: stamp the owning package on every WitFunction so
    // emit_lib can route imports per-shim instead of hardcoding
    // `bindings::postgis::wasm::...`.
    let (pkg_ns_name, pkg_version) = parse_package_decl(text)
        .unwrap_or_else(|| ("unknown:unknown".to_string(), "0.0.0".to_string()));

    let mut current_interface: Option<String> = None;
    let mut depth_inside_interface: i32 = 0;
    // Round-490: track whether the current line is inside a
    // `resource NAME { ... }` block. Resource-method declarations
    // look syntactically identical to interface-level free functions
    // (`width: func() -> u32;`), but they're NOT callable as
    // `module::width(...)` — they're called via a resource handle
    // (`raster.width()`). #547 (W3.1) captures them with
    // `resource = Some(<resource_kebab>)` instead of dropping; the
    // dispatcher's resource-method index keys on
    // `<resource>_<method_snake>` separately from the free-function
    // index so the prefix-strip lookup (`srid` → `st_srid`) can't
    // confuse a method for a free function.
    let mut depth_inside_resource: i32 = 0;
    let mut current_resource: Option<String> = None;

    // Accumulator for multi-line function declarations. We flush
    // it once we hit a `;` (function terminator) at top-level
    // depth (i.e. not nested inside a record / variant block).
    let mut pending: Option<String> = None;

    for raw_line in stripped.lines() {
        // Drop line comments and trim.
        let line = strip_line_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if current_interface.is_none() {
            if let Some(name) = parse_interface_open(line) {
                current_interface = Some(name);
                depth_inside_interface = 1;
                continue;
            }
        } else {
            // If we're accumulating a multi-line function decl,
            // keep appending until we see the `;` terminator.
            if let Some(ref mut acc) = pending {
                acc.push(' ');
                acc.push_str(line);
                if line.ends_with(';') {
                    let collapsed = pending.take().unwrap();
                    if let Some(func) = parse_func_line(&collapsed) {
                        out.push(WitFunction {
                            package: pkg_ns_name.clone(),
                            package_version: pkg_version.clone(),
                            interface: current_interface.clone().unwrap(),
                            kebab_name: func.0,
                            params: func.1,
                            ret: func.2,
                            resource: None,
                            is_constructor: false,
                        });
                    }
                }
                continue;
            }

            // Track braces so `record { ... }` / `resource { ... }`
            // blocks inside the interface don't fool us into thinking
            // the interface ended at their `}`.
            let opens = line.matches('{').count() as i32;
            let closes = line.matches('}').count() as i32;

            // Round-490: detect `resource NAME {` openings and bump
            // the resource-depth counter. A `resource NAME;` (no
            // body) doesn't open a block so the counter stays at 0.
            // #547 (W3.1): also remember the resource name so
            // function lines inside the block can be tagged as
            // methods rather than dropped.
            if depth_inside_resource == 0 {
                if let Some(rname) = parse_resource_decl(line) {
                    if opens > 0 {
                        depth_inside_resource = opens - closes;
                        depth_inside_interface += opens - closes;
                        current_resource = Some(rname);
                        continue;
                    }
                    // `resource NAME;` (no body) — no methods to capture.
                }
            }
            if depth_inside_resource > 0 {
                // #547 (W3.1): inside a resource body, recognise
                // method-shaped lines and capture them as
                // WitFunction with `resource = Some(...)`. Methods
                // look syntactically identical to interface-level
                // free functions but their dispatch goes through
                // the resource-method index keyed by
                // `<resource>_<method_snake>`, never through
                // index_wit_fns / wit_nohyphen.
                if let Some(ref mut acc) = pending {
                    // We're mid multi-line method decl.
                    acc.push(' ');
                    acc.push_str(line);
                    if line.ends_with(';') {
                        let collapsed = pending.take().unwrap();
                        if let Some(func) = parse_func_line(&collapsed) {
                            out.push(WitFunction {
                                package: pkg_ns_name.clone(),
                                package_version: pkg_version.clone(),
                                interface: current_interface.clone().unwrap(),
                                kebab_name: func.0,
                                params: func.1,
                                ret: func.2,
                                resource: current_resource.clone(),
                                is_constructor: false,
                            });
                        }
                    }
                } else if !line.starts_with("//") {
                    // #556 (W3.1 mop-up): recognise `constructor(...)`
                    // lines inside the resource body. Single-line form
                    // only — postgis-topology-types' constructor fits
                    // on one line; multi-line constructors are not
                    // observed in the postgis WIT today.
                    if let Some(rkebab) = current_resource.clone() {
                        if let Some((params, ret)) = parse_constructor_line(line, &rkebab) {
                            out.push(WitFunction {
                                package: pkg_ns_name.clone(),
                                package_version: pkg_version.clone(),
                                interface: current_interface.clone().unwrap(),
                                // Synthesise kebab `create-<resource>`
                                // so the SQL-name lookups
                                // (`st_createtopology`,
                                // `st_create_topology`, etc.) resolve
                                // through the existing snake /
                                // no-hyphen indexes — no per-name
                                // override needed.
                                kebab_name: format!("create-{rkebab}"),
                                params,
                                ret,
                                resource: Some(rkebab.clone()),
                                is_constructor: true,
                            });
                            depth_inside_resource += opens - closes;
                            depth_inside_interface += opens - closes;
                            if depth_inside_resource <= 0 {
                                depth_inside_resource = 0;
                                current_resource = None;
                            }
                            if depth_inside_interface <= 0 {
                                current_interface = None;
                                depth_inside_interface = 0;
                                pending = None;
                                current_resource = None;
                            }
                            continue;
                        }
                    }
                    if find_func_token(line).is_some() && !line.ends_with(';') {
                        pending = Some(line.to_string());
                    } else if let Some(func) = parse_func_line(line) {
                        out.push(WitFunction {
                            package: pkg_ns_name.clone(),
                            package_version: pkg_version.clone(),
                            interface: current_interface.clone().unwrap(),
                            kebab_name: func.0,
                            params: func.1,
                            ret: func.2,
                            resource: current_resource.clone(),
                            is_constructor: false,
                        });
                    }
                }
                depth_inside_resource += opens - closes;
                depth_inside_interface += opens - closes;
                if depth_inside_resource <= 0 {
                    depth_inside_resource = 0;
                    current_resource = None;
                }
                if depth_inside_interface <= 0 {
                    current_interface = None;
                    depth_inside_interface = 0;
                    pending = None;
                    current_resource = None;
                }
                continue;
            }

            // Try to recognise a function declaration BEFORE
            // adjusting depth — function lines may end with `;`
            // and contain no braces.
            if !line.starts_with("//") {
                if find_func_token(line).is_some() && !line.ends_with(';') {
                    // Start of a multi-line function decl.
                    pending = Some(line.to_string());
                } else if let Some(func) = parse_func_line(line) {
                    out.push(WitFunction {
                        package: pkg_ns_name.clone(),
                        package_version: pkg_version.clone(),
                        interface: current_interface.clone().unwrap(),
                        kebab_name: func.0,
                        params: func.1,
                        ret: func.2,
                        resource: None,
                        is_constructor: false,
                    });
                }
            }

            depth_inside_interface += opens - closes;
            if depth_inside_interface <= 0 {
                current_interface = None;
                depth_inside_interface = 0;
                pending = None;
            }
        }
    }
}

/// #556 (W3.1 mop-up): parse a `constructor(args)` line inside a
/// resource body. Returns `Some((params, ret))` when `line` matches
/// the constructor shape, where `ret` is synthesised as the named
/// resource (constructors implicitly return `Self`). Returns None
/// for non-constructor lines so the regular `parse_func_line` path
/// can handle them.
fn parse_constructor_line(line: &str, resource_kebab: &str) -> Option<(Vec<WitParam>, WitRet)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("constructor")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    // Match the closing paren that ends the parameter list, tracking
    // nested angle brackets / parens so generic types like
    // `list<u8>` don't trip the scan.
    let mut depth = 1;
    let mut end = None;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '<' => depth += 1,
            ')' | '>' => {
                depth -= 1;
                if depth == 0 && c == ')' {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let params_str = &rest[..end];
    let params = parse_params(params_str);
    // wit-bindgen lowers the constructor's implicit return to the
    // resource type itself. The dispatcher's classify_return then
    // routes it through the resource-specific blob encode helper
    // (`TopologyBlob` → `.to_bytes()`, etc.).
    let ret = WitRet {
        inner: parse_type(resource_kebab),
        fallible: false,
        error_ty: None,
    };
    Some((params, ret))
}

/// Replace `/* ... */` blocks with whitespace of the same length
/// so line numbers / column positions stay stable.
fn strip_block_comments(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Find closing */
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                if bytes[j] == b'\n' {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                j += 1;
            }
            if j + 1 < bytes.len() {
                out.push(' ');
                out.push(' ');
                i = j + 2;
            } else {
                i = bytes.len();
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn strip_line_comment(line: &str) -> &str {
    if let Some(idx) = line.find("//") {
        &line[..idx]
    } else {
        line
    }
}

/// Returns `Some(interface_name)` when `line` opens an interface
/// block (e.g. `interface postgis-measurements {`).
fn parse_interface_open(line: &str) -> Option<String> {
    let rest = line.strip_prefix("interface ")?;
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '{')
        .unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Locate the `:` of a `<kebab-name>: <ws>* func(` declaration on
/// `line`, returning its byte offset. The whitespace run between
/// the colon and the `func(` keyword may be zero or more horizontal
/// whitespace characters (spaces or tabs). Lines without a `func(`
/// keyword anywhere after a candidate `:` return None.
fn find_func_token(line: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(':') {
        let colon_at = search_from + rel;
        let after = &line[colon_at + 1..];
        let trimmed = after.trim_start_matches(|c: char| c == ' ' || c == '\t');
        if trimmed.starts_with("func(") {
            return Some(colon_at);
        }
        search_from = colon_at + 1;
        if search_from >= line.len() {
            break;
        }
    }
    None
}

/// Returns `Some((kebab_name, params, ret))` when `line` declares
/// a function. Only one-line function declarations are recognised;
/// the postgis-wasm WIT happens to write every function on one
/// line, so this is sufficient for Phase 3.
fn parse_func_line(line: &str) -> Option<(String, Vec<WitParam>, WitRet)> {
    // Pattern: `<kebab-name>: <ws>* func(...): ...;`. Some WIT
    // files (notably mobilitydb-wasm's temporal.wit) right-align
    // the `func(` token after the `:` with extra spaces for visual
    // tidiness — e.g. `tfloat-abs:    func(...)`. Original Phase 3
    // matched only the single-space form (`: func(`) and silently
    // skipped any line using extra whitespace; W1 widens the
    // matcher to accept any run of whitespace between `:` and the
    // `func(` token.
    let func_idx = find_func_token(line)?;
    let kebab = line[..func_idx].trim().to_string();
    if !is_kebab_ident(&kebab) {
        return None;
    }

    // Skip past `:`, the whitespace run, and `func(`.
    let paren_after = line[func_idx + 1..].find("func(")?;
    let after = &line[func_idx + 1 + paren_after + "func(".len()..];
    // Match the closing paren that ends the parameter list,
    // tracking nested angle brackets / parens (option<tuple<...>>
    // shows up in some files).
    let mut depth = 1;
    let mut end = None;
    for (i, c) in after.char_indices() {
        match c {
            '(' | '<' => depth += 1,
            ')' | '>' => {
                depth -= 1;
                if depth == 0 && c == ')' {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let params_str = &after[..end];
    let after_params = &after[end + 1..];

    let params = parse_params(params_str);

    // Look for ` -> ` followed by a type expression terminated by `;`.
    let ret = parse_return(after_params);

    Some((kebab, params, ret))
}

fn parse_return(after: &str) -> WitRet {
    let after = after.trim_start();
    let body = if let Some(rest) = after.strip_prefix("-> ") {
        rest
    } else if let Some(rest) = after.strip_prefix("->") {
        rest
    } else {
        // No return clause — implicit `()`. Treated as Unsupported
        // for Phase 3 (no PostGIS scalar truly has void return).
        return WitRet {
            inner: WitType::Unsupported(after.trim().to_string()),
            fallible: false,
            error_ty: None,
        };
    };
    let body = body.trim().trim_end_matches(';').trim().to_string();
    parse_return_body(&body)
}

fn parse_return_body(body: &str) -> WitRet {
    if let Some(inside) = strip_wrapper(body, "result<") {
        // result<T, postgis-error> — split at top-level comma to
        // pull both sides. #565 (#557fix.2): keep the error half so
        // the reachability walker can mark `postgis-error` /
        // `temporal-error` / etc. as live even though `WitRet.inner`
        // only carries the OK type.
        let parts = split_top_level_comma(&inside);
        let inner = parts
            .get(0)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let inner_ty = parse_type(&inner);
        let error_ty = parts.get(1).map(|s| parse_type(s.trim()));
        return WitRet {
            inner: inner_ty,
            fallible: true,
            error_ty,
        };
    }
    WitRet {
        inner: parse_type(body),
        fallible: false,
        error_ty: None,
    }
}

fn parse_params(s: &str) -> Vec<WitParam> {
    let parts = split_top_level_comma(s);
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let p = p.trim();
            // `name: type`
            let colon = p.find(':');
            match colon {
                Some(c) => WitParam {
                    name: p[..c].trim().to_string(),
                    ty: parse_type(p[c + 1..].trim()),
                },
                None => WitParam {
                    name: String::new(),
                    ty: parse_type(p),
                },
            }
        })
        .collect()
}

fn parse_type(s: &str) -> WitType {
    let s = s.trim();
    if let Some(inner) = strip_wrapper(s, "borrow<") {
        return match parse_type(&inner) {
            WitType::Geometry { .. } => WitType::Geometry { borrowed: true },
            WitType::Geography { .. } => WitType::Geography { borrowed: true },
            WitType::Raster { .. } => WitType::Raster { borrowed: true },
            WitType::Topology { .. } => WitType::Topology { borrowed: true },
            other => WitType::Unsupported(format!("borrow<{}>", type_label(&other))),
        };
    }
    if let Some(inner) = strip_wrapper(s, "option<") {
        let inner_ty = parse_type(&inner);
        // Phase F (#522): KEEP the structure even when the inner
        // resolves to `Unsupported(name)` — the dispatcher routes
        // `Option(Unsupported(name))` to wit-value if `name`
        // matches a record in the registry. Pre-Phase-F this case
        // collapsed to `Unsupported("option<name>")` and lost the
        // structure, so option<record> never reached the
        // record-matching path.
        return WitType::Option(Box::new(inner_ty));
    }
    if s == "list<u8>" {
        return WitType::ListU8;
    }
    if let Some(inner) = strip_wrapper(s, "list<") {
        let inner_ty = parse_type(&inner);
        match inner_ty {
            WitType::Geometry { borrowed: true } => return WitType::ListGeomBorrow,
            WitType::Geometry { borrowed: false } => return WitType::ListGeomOwned,
            // #548 (W3.2): raster equivalent of ListGeomBorrow,
            // surfaces in `st-rast-union-aggregate`.
            WitType::Raster { borrowed: true } => return WitType::ListRasterBorrow,
            WitType::Option(ref inner2) if matches!(**inner2, WitType::U32) => {
                return WitType::ListOptionU32;
            }
            // W2 Phase 2 (#553): keep the `List(Box<T>)` wrapper even
            // when `T` is `Unsupported(name)`. Pre-Phase-2 we collapsed
            // to `Unsupported("list<name>")` which lost the structure,
            // so a record-typed list element (e.g. `list<int-span>`)
            // never reached the dispatcher's record-registry lookup.
            // The classify_param `List(Unsupported(name))` arm now
            // routes through the record registry when `name` matches
            // a record's kebab.
            //
            // Phase F (#522): everything else surfaces as the generic
            // `List(Box<T>)`. The dispatcher decides whether to wire
            // it (records → wit-value, primitives → JSON list,
            // returns → First* projection).
            _ => return WitType::List(Box::new(inner_ty)),
        }
    }
    // Phase F (#522): nested `result<T, E>` — top-level result
    // is still stripped by `parse_return_body` into `fallible`,
    // but a nested result (inside option or tuple) surfaces here.
    if let Some(inside) = strip_wrapper(s, "result<") {
        let parts = split_top_level_comma(&inside);
        let ok_ty = parse_type(parts.get(0).map(|s| s.trim()).unwrap_or(""));
        let err_ty = parse_type(parts.get(1).map(|s| s.trim()).unwrap_or(""));
        if matches!(ok_ty, WitType::Unsupported(_)) {
            return WitType::Unsupported(format!("result<{}>", type_label(&ok_ty)));
        }
        return WitType::Result(Box::new(ok_ty), Box::new(err_ty));
    }
    if let Some(inner) = strip_wrapper(s, "tuple<") {
        // Round 3: heterogeneous N-tuple. Recognised so that
        // `option<tuple<...>>` params classify as `Option(Tuple)`
        // → OptionNone dispatch, and the specific shape
        // `tuple<bool, option<string>, option<geometry>>` (from
        // `st-is-valid-detail`) classifies as a tuple return.
        let parts = split_top_level_comma(&inner);
        let elems: Vec<WitType> = parts
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| parse_type(p.trim()))
            .collect();
        if elems.iter().any(|t| matches!(t, WitType::Unsupported(_))) {
            return WitType::Unsupported(format!("tuple<{}>", inner.trim()));
        }
        return WitType::Tuple(elems);
    }
    match s {
        "geometry" => WitType::Geometry { borrowed: false },
        "geography" => WitType::Geography { borrowed: false },
        "raster" => WitType::Raster { borrowed: false },
        "topology" => WitType::Topology { borrowed: false },
        "string" => WitType::String,
        "f64" => WitType::F64,
        "f32" => WitType::F32,
        "s32" => WitType::S32,
        "s64" => WitType::S64,
        "u32" => WitType::U32,
        "u64" => WitType::U64,
        "u8" => WitType::U8,
        "bool" => WitType::Bool,
        "bbox" => WitType::Bbox,
        other => WitType::Unsupported(other.to_string()),
    }
}

/// Public façade — used by `dispatch::classify_udtf_output_row` to
/// re-parse a record field's raw type text into the dispatcher's
/// `WitType` alphabet so column affinity can be derived per field.
/// Task #531.
pub fn parse_type_public(s: &str) -> WitType {
    parse_type(s)
}

/// Recursively substitute every `Unsupported(name)` whose `name`
/// matches a `type X = body;` alias's kebab name by parsing the
/// alias body as a `WitType`. Walks into `Option`, `List`,
/// `Result`, `Tuple`. Phase F (#522): mobilitydb's
/// `type timestamp-tz = s64;` is the primary motivator — without
/// this resolution every `timestamp-tz` param/return failed to
/// classify because the bare alias name surfaced as
/// `Unsupported("timestamp-tz")`.
///
/// Loops are guarded by a depth counter — pathological aliases that
/// refer back to themselves stop substituting at depth 16.
pub fn resolve_aliases(ty: WitType, aliases: &[WitTypeAlias]) -> WitType {
    fn go(ty: WitType, aliases: &[WitTypeAlias], depth: u32) -> WitType {
        if depth > 16 {
            return ty;
        }
        match ty {
            WitType::Option(inner) => WitType::Option(Box::new(go(*inner, aliases, depth + 1))),
            WitType::List(inner) => WitType::List(Box::new(go(*inner, aliases, depth + 1))),
            WitType::Result(ok, err) => WitType::Result(
                Box::new(go(*ok, aliases, depth + 1)),
                Box::new(go(*err, aliases, depth + 1)),
            ),
            WitType::Tuple(elems) => WitType::Tuple(
                elems.into_iter().map(|e| go(e, aliases, depth + 1)).collect(),
            ),
            WitType::Unsupported(name) => {
                if let Some(a) = aliases.iter().find(|a| a.kebab_name == name) {
                    let parsed = parse_type(&a.body);
                    return go(parsed, aliases, depth + 1);
                }
                WitType::Unsupported(name)
            }
            other => other,
        }
    }
    go(ty, aliases, 0)
}

fn type_label(ty: &WitType) -> String {
    match ty {
        WitType::Geometry { .. } => "geometry".into(),
        WitType::Geography { .. } => "geography".into(),
        WitType::Raster { .. } => "raster".into(),
        WitType::Topology { .. } => "topology".into(),
        WitType::String => "string".into(),
        WitType::F64 => "f64".into(),
        WitType::F32 => "f32".into(),
        WitType::S32 => "s32".into(),
        WitType::S64 => "s64".into(),
        WitType::U32 => "u32".into(),
        WitType::U64 => "u64".into(),
        WitType::U8 => "u8".into(),
        WitType::Bool => "bool".into(),
        WitType::ListU8 => "list<u8>".into(),
        WitType::ListGeomBorrow => "list<borrow<geometry>>".into(),
        WitType::ListRasterBorrow => "list<borrow<raster>>".into(),
        WitType::ListGeomOwned => "list<geometry>".into(),
        WitType::ListOptionU32 => "list<option<u32>>".into(),
        WitType::Option(inner) => format!("option<{}>", type_label(inner)),
        WitType::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(type_label).collect();
            format!("tuple<{}>", parts.join(", "))
        }
        WitType::Bbox => "bbox".into(),
        WitType::List(inner) => format!("list<{}>", type_label(inner)),
        WitType::Result(ok, _err) => format!("result<{}>", type_label(ok)),
        WitType::Unsupported(s) => s.clone(),
    }
}

/// Returns the inside of `wrap...>` if `s` matches; assumes
/// the wrapper is `name<` and the matching close is the LAST `>`.
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

/// Split at commas not inside `<...>` or `(...)`.
fn split_top_level_comma(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' => depth += 1,
            b'>' | b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        out.push(s[start..].to_string());
    }
    out
}

fn is_kebab_ident(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() {
        return false;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Convert kebab-case `st-geom-from-text` to snake_case
/// `st_geom_from_text` (the canonical name shape SQL callers /
/// the interface DB use for "underscored long form" lookups).
pub fn kebab_to_snake(s: &str) -> String {
    s.replace('-', "_")
}

/// Convert snake_case (the interface DB's no-underscore canonical
/// name, e.g. `st_makepoint`) into the underscored long form
/// (`st_make_point`). Used only as a fallback; the interface DB's
/// alias table is the authoritative join.
#[allow(dead_code)]
pub fn snake_to_underscored(s: &str) -> String {
    // Identity for Phase 3 — the actual snake → kebab translation
    // happens through the alias table, not here.
    s.to_string()
}

/// Map a WIT interface name to its Rust-binding module alias as
/// used inside the emitted `lib.rs`. Hand-curated postgis-bridge
/// aliases take precedence; everything else falls back to the
/// kebab → snake_case conversion (Phase D).
pub fn interface_to_rust_alias(interface: &str) -> Option<String> {
    if let Some(&s) = POSTGIS_INTERFACE_ALIASES
        .iter()
        .find(|(iface, _)| *iface == interface)
        .map(|(_, alias)| alias)
    {
        return Some(s.to_string());
    }
    // Algorithmic fallback: snake_case the kebab interface name.
    // E.g. `tint-ops` → `tint_ops`, `tfloat-ops` → `tfloat_ops`.
    // Interfaces with empty or non-identifier names are filtered out.
    if !is_kebab_ident(interface) {
        return None;
    }
    Some(interface.replace('-', "_"))
}

/// Inverse of `interface_to_rust_alias`. Hand-curated postgis-bridge
/// aliases reverse-map to the corresponding postgis_* module ident;
/// everything else assumes the alias IS the module ident (the
/// algorithmic fallback above is its own inverse). Phase D.
pub fn alias_to_wit_module_ident(alias: &str) -> Option<String> {
    if let Some(&s) = POSTGIS_ALIAS_TO_MODULE
        .iter()
        .find(|(a, _)| *a == alias)
        .map(|(_, module)| module)
    {
        return Some(s.to_string());
    }
    // Algorithmic fallback: the alias IS the module ident (both
    // are snake_case).
    if !is_snake_ident(alias) {
        return None;
    }
    Some(alias.to_string())
}

fn is_snake_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// #565 (#557fix.2): compute the closure of primary-shim record /
/// variant / enum / flags types that are REACHABLE through the
/// imported function set.
///
/// Motivation: wit-component (run during the wasm32-wasip2 lower
/// step) trims the bridge's component imports to only those
/// functions/types referenced by the lowered core wasm. wit-bindgen
/// in turn only emits Rust struct/enum/etc. definitions for
/// imported types referenced by at least one imported function
/// signature (transitively through reachable record fields).
///
/// The force-link block in `emit_lib::render_force_link_upstream_imports`
/// needs to reference EVERY type wit-component sees in the upstream
/// import shape — including types the bridge never calls directly,
/// to keep them live for wac plug's structural match. But the block
/// CAN'T reference types wit-bindgen has DCE'd out — the
/// `bindings::<pkg>::<iface>::<TypeName>` path won't compile.
///
/// This function bridges the two by walking the imported function
/// set and computing the same reachable-type closure wit-bindgen
/// applies. The emitter then filters its type-ref list by membership
/// in the returned set.
///
/// Reachability rules:
///   - Walk every WitFunction whose package belongs to `primary`.
///     This covers free functions, resource methods, and resource
///     constructors (the latter two reach their param/return types
///     just like free functions in wit-bindgen's Rust emit).
///   - For each function, collect referenced kebab type names from
///     params, return inner, AND the parsed error type (preserved
///     by #565's WitRet.error_ty field).
///   - For each newly-reached primary record, walk its raw field
///     type texts via `parse_type` and collect more names.
///   - Variants, enums, and flags are leaves — when referenced by
///     name they're added to the set, but the parser doesn't
///     capture variant case payloads so no further recursion
///     happens through them. In practice the upstream WIT keeps
///     variant payloads scalar (string / primitive) so this is
///     correct for today's shims; a future shim that puts records
///     inside variant cases would need this extended.
///
/// Returns a deterministic `BTreeSet<(package, interface,
/// kebab_name)>` keyed identically to the emitter's per-package
/// iteration order so the membership filter is a direct lookup.
pub fn reachable_primary_types(
    fns: &[WitFunction],
    packages: &[WitPackage],
    primary: &str,
) -> std::collections::BTreeSet<(String, String, String)> {
    use std::collections::{BTreeMap, BTreeSet};

    // Index primary records by (interface, kebab) → (package,
    // field-type-texts). Two interfaces can declare records with the
    // same kebab name (mobilitydb's `stbox-ops::stbox3d` vs
    // `stbox3d-ops::stbox3d`); wit-bindgen emits both as distinct
    // Rust types at separate module paths so the force-link block
    // must keep both — keying by interface preserves the distinction.
    let mut record_index: BTreeMap<(String, String), (String, Vec<String>)> = BTreeMap::new();
    // Variants / enums / flags → (package). No body to recurse on.
    let mut leaf_index: BTreeMap<(String, String), String> = BTreeMap::new();
    // Auxiliary kebab → list of (interface) for cross-interface
    // fallback when a function's bare-name reference doesn't match
    // a record in its own interface. The WIT parser doesn't track
    // `use` clauses; this fallback approximates them by claiming
    // every same-named record in the primary package as potentially
    // reached when same-interface lookup misses.
    let mut record_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut leaf_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pkg in packages {
        if pkg.ns_name.split(':').next() != Some(primary) {
            continue;
        }
        for r in &pkg.records {
            record_index.insert(
                (r.interface.clone(), r.kebab_name.clone()),
                (
                    pkg.ns_name.clone(),
                    r.fields.iter().map(|(_, ty)| ty.clone()).collect(),
                ),
            );
            record_by_name
                .entry(r.kebab_name.clone())
                .or_default()
                .push(r.interface.clone());
        }
        for v in &pkg.variants {
            leaf_index.insert(
                (v.interface.clone(), v.kebab_name.clone()),
                pkg.ns_name.clone(),
            );
            leaf_by_name
                .entry(v.kebab_name.clone())
                .or_default()
                .push(v.interface.clone());
        }
        for e in &pkg.enums {
            leaf_index.insert(
                (e.interface.clone(), e.kebab_name.clone()),
                pkg.ns_name.clone(),
            );
            leaf_by_name
                .entry(e.kebab_name.clone())
                .or_default()
                .push(e.interface.clone());
        }
        for f in &pkg.flags {
            leaf_index.insert(
                (f.interface.clone(), f.kebab_name.clone()),
                pkg.ns_name.clone(),
            );
            leaf_by_name
                .entry(f.kebab_name.clone())
                .or_default()
                .push(f.interface.clone());
        }
    }

    let mut reachable: BTreeSet<(String, String, String)> = BTreeSet::new();
    // Queue carries the (scoping interface, ref name) pair so the
    // resolver can prefer a same-interface declaration.
    let mut queue: Vec<(String, String)> = Vec::new();

    // Seed: every primary-package function's params + return + error.
    // Scope each name with the function's owning interface so
    // bare-name references inside an interface resolve to that
    // interface's declarations first.
    for f in fns {
        if f.package.split(':').next() != Some(primary) {
            continue;
        }
        let scope = f.interface.clone();
        let mut names: Vec<String> = Vec::new();
        for p in &f.params {
            collect_named_types(&p.ty, &mut names);
        }
        collect_named_types(&f.ret.inner, &mut names);
        if let Some(err_ty) = &f.ret.error_ty {
            collect_named_types(err_ty, &mut names);
        }
        for n in names {
            queue.push((scope.clone(), n));
        }
    }

    while let Some((scope, name)) = queue.pop() {
        // Helper: enqueue field types of a newly-reached record
        // under that record's own interface scope (its field bare
        // names resolve against its own interface first).
        let mut walk_record_fields = |record_iface: &str, field_types: &[String], q: &mut Vec<(String, String)>| {
            for raw in field_types {
                let ty = parse_type(raw);
                let mut names: Vec<String> = Vec::new();
                collect_named_types(&ty, &mut names);
                for n in names {
                    q.push((record_iface.to_string(), n));
                }
            }
        };

        // 1. Same-interface record? Use it.
        if let Some((pkg, fields)) = record_index.get(&(scope.clone(), name.clone())) {
            let key = (pkg.clone(), scope.clone(), name.clone());
            if reachable.insert(key) {
                walk_record_fields(&scope, fields, &mut queue);
            }
            continue;
        }
        // 2. Same-interface variant / enum / flags? Use it.
        if let Some(pkg) = leaf_index.get(&(scope.clone(), name.clone())) {
            reachable.insert((pkg.clone(), scope.clone(), name.clone()));
            continue;
        }
        // 3. Cross-interface fallback (approximating `use` clauses):
        //    if the name only exists in other interfaces of the
        //    primary's packages, mark all matching declarations as
        //    reached. wit-bindgen would emit any of them that some
        //    function actually references; over-marking here is OK
        //    as long as we don't reach unreachable declarations —
        //    which we don't, because the cross-interface fallback
        //    only fires when no same-interface match exists for
        //    THIS bare-name reference.
        if let Some(ifaces) = record_by_name.get(&name) {
            for iface in ifaces {
                if let Some((pkg, fields)) = record_index.get(&(iface.clone(), name.clone())) {
                    let key = (pkg.clone(), iface.clone(), name.clone());
                    if reachable.insert(key) {
                        walk_record_fields(iface, fields, &mut queue);
                    }
                }
            }
            continue;
        }
        if let Some(ifaces) = leaf_by_name.get(&name) {
            for iface in ifaces {
                if let Some(pkg) = leaf_index.get(&(iface.clone(), name.clone())) {
                    reachable.insert((pkg.clone(), iface.clone(), name.clone()));
                }
            }
        }
    }

    reachable
}

/// Helper for `reachable_primary_types`: pull every named kebab
/// type reference out of a `WitType` tree.
///
/// Resources (`geometry`, `geography`, `raster`, `topology`) and
/// primitives don't contribute names. Specialised list shapes
/// (`ListGeomBorrow`, `ListU8`, etc.) wrap resource / primitive
/// elements so they don't either. `Bbox` is the one named record
/// the parser recognises as a dedicated variant rather than via
/// `Unsupported`; emit "bbox" so the postgis-types `bbox` record
/// counts as reached when a function returns it.
fn collect_named_types(ty: &WitType, out: &mut Vec<String>) {
    match ty {
        WitType::Bbox => out.push("bbox".to_string()),
        WitType::Unsupported(name) => out.push(name.clone()),
        WitType::Option(inner) => collect_named_types(inner, out),
        WitType::List(inner) => collect_named_types(inner, out),
        WitType::Result(ok, err) => {
            collect_named_types(ok, out);
            collect_named_types(err, out);
        }
        WitType::Tuple(elems) => {
            for t in elems {
                collect_named_types(t, out);
            }
        }
        // Resources, primitives, and the specialised list/option
        // wrappers don't contribute named type references.
        _ => {}
    }
}

const POSTGIS_INTERFACE_ALIASES: &[(&str, &str)] = &[
    ("postgis-accessors", "pg_acc"),
    ("postgis-aggregates", "pg_agg"),
    ("postgis-clustering", "pg_cluster"),
    ("postgis-constructors", "pg_ctor"),
    ("postgis-geocoder", "pg_geo"),
    ("postgis-geodetic", "pg_geog"),
    ("postgis-linear-ref", "pg_lin"),
    ("postgis-measurements", "pg_meas"),
    ("postgis-operators", "pg_op"),
    ("postgis-output", "pg_out"),
    ("postgis-predicates", "pg_pred"),
    ("postgis-processing", "pg_proc"),
    // Round-490: raster + topology *types* interfaces hold the
    // resource definitions plus the free `from-binary` / `from-bytes`
    // decoders. The aliases let the emitter compose
    // `pg_rast_types::from_binary(...)` and
    // `pg_topo_types::from_bytes(...)` in the param decode body.
    ("postgis-raster-types", "pg_rast_types"),
    ("postgis-topology-types", "pg_topo_types"),
    ("postgis-raster-accessors", "pg_rast_acc"),
    ("postgis-raster-constructors", "pg_rast_ctor"),
    ("postgis-raster-mapalgebra", "pg_rast_ma"),
    ("postgis-raster-output", "pg_rast_out"),
    ("postgis-raster-pixels", "pg_rast_px"),
    ("postgis-raster-predicates", "pg_rast_pred"),
    ("postgis-raster-processing", "pg_rast_proc"),
    ("postgis-raster-stats", "pg_rast_stats"),
    ("postgis-raster-vector", "pg_rast_vec"),
    ("postgis-raster-aggregates", "pg_rast_agg"),
    ("postgis-sfcgal", "pg_sfcgal"),
    ("postgis-spatial-index", "pg_strtree"),
    ("postgis-three-d", "pg_threed"),
    ("postgis-topology-edit", "pg_topo_edit"),
    ("postgis-topology-output", "pg_topo_out"),
    ("postgis-topology-query", "pg_topo_query"),
    ("postgis-topology-topogeom", "pg_topogeom"),
    ("postgis-transformations", "pg_xform"),
];

const POSTGIS_ALIAS_TO_MODULE: &[(&str, &str)] = &[
    ("pg_acc", "postgis_accessors"),
    ("pg_agg", "postgis_aggregates"),
    ("pg_cluster", "postgis_clustering"),
    ("pg_ctor", "postgis_constructors"),
    ("pg_geo", "postgis_geocoder"),
    ("pg_geog", "postgis_geodetic"),
    ("pg_lin", "postgis_linear_ref"),
    ("pg_meas", "postgis_measurements"),
    ("pg_op", "postgis_operators"),
    ("pg_out", "postgis_output"),
    ("pg_pred", "postgis_predicates"),
    ("pg_proc", "postgis_processing"),
    ("pg_rast_types", "postgis_raster_types"),
    ("pg_topo_types", "postgis_topology_types"),
    ("pg_rast_acc", "postgis_raster_accessors"),
    ("pg_rast_ctor", "postgis_raster_constructors"),
    ("pg_rast_ma", "postgis_raster_mapalgebra"),
    ("pg_rast_out", "postgis_raster_output"),
    ("pg_rast_px", "postgis_raster_pixels"),
    ("pg_rast_pred", "postgis_raster_predicates"),
    ("pg_rast_proc", "postgis_raster_processing"),
    ("pg_rast_stats", "postgis_raster_stats"),
    ("pg_rast_vec", "postgis_raster_vector"),
    ("pg_rast_agg", "postgis_raster_aggregates"),
    ("pg_sfcgal", "postgis_sfcgal"),
    ("pg_strtree", "postgis_spatial_index"),
    ("pg_threed", "postgis_three_d"),
    ("pg_topo_edit", "postgis_topology_edit"),
    ("pg_topo_out", "postgis_topology_output"),
    ("pg_topo_query", "postgis_topology_query"),
    ("pg_topogeom", "postgis_topology_topogeom"),
    ("pg_xform", "postgis_transformations"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_interface() {
        let src = r#"
package postgis:wasm@0.1.0;

/// PostGIS measurement functions for geometric calculations
interface postgis-measurements {
    use postgis-types.{geometry, postgis-error};

    /// ST_Area - Calculate area of a geometry
    st-area: func(geom: borrow<geometry>) -> result<f64, postgis-error>;

    /// ST_Distance - distance between two geometries
    st-distance: func(geom1: borrow<geometry>, geom2: borrow<geometry>) -> result<f64, postgis-error>;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        assert_eq!(out.len(), 2, "got {:?}", out);
        assert_eq!(out[0].interface, "postgis-measurements");
        assert_eq!(out[0].kebab_name, "st-area");
        assert_eq!(out[0].params.len(), 1);
        assert_eq!(out[0].params[0].name, "geom");
        assert!(matches!(out[0].params[0].ty, WitType::Geometry { borrowed: true }));
        assert!(out[0].ret.fallible);
        assert!(matches!(out[0].ret.inner, WitType::F64));
        assert_eq!(out[1].kebab_name, "st-distance");
        assert_eq!(out[1].params.len(), 2);
    }

    #[test]
    fn parses_record_block_without_confusion() {
        let src = r#"
interface postgis-aggregates {
    record bbox3d {
        min-x: f64,
        min-y: f64,
        max-x: f64,
    }
    st-extent-threed: func(geoms: list<borrow<geometry>>) -> bbox3d;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        // The record block's contents (`min-x: f64,` etc.) must
        // not be misread as function declarations because they
        // don't carry `: func(`.
        assert_eq!(out.len(), 1, "got {:?}", out);
        assert_eq!(out[0].kebab_name, "st-extent-threed");
    }

    #[test]
    fn kebab_to_snake_works() {
        assert_eq!(kebab_to_snake("st-geom-from-text"), "st_geom_from_text");
        assert_eq!(kebab_to_snake("st-area"), "st_area");
    }

    #[test]
    fn parses_multi_line_function_decl() {
        let src = r#"
interface postgis-aggregates {
    use postgis-types.{geometry, postgis-error};

    /// Aggregate cluster-within
    st-cluster-within-aggregate: func(
        geoms: list<borrow<geometry>>,
        distance: f64,
    ) -> result<list<geometry>, postgis-error>;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        assert_eq!(out.len(), 1, "got {:?}", out);
        assert_eq!(out[0].kebab_name, "st-cluster-within-aggregate");
        assert_eq!(out[0].params.len(), 2);
        assert!(matches!(out[0].params[0].ty, WitType::ListGeomBorrow));
        assert!(matches!(out[0].params[1].ty, WitType::F64));
        assert!(out[0].ret.fallible);
    }

    #[test]
    fn parses_bbox_and_tuple_round3_shapes() {
        // Round 3: bbox record return + tuple<...> return + option<tuple<...>> param.
        let src = r#"
interface postgis-constructors {
    use postgis-types.{geometry, bbox, postgis-error};
    st-make-box2d: func(low-left: borrow<geometry>, up-right: borrow<geometry>) -> result<bbox, postgis-error>;
    st-box-from-geohash: func(geohash-str: string, precision: option<u32>) -> result<bbox, postgis-error>;
    st-tile-envelope: func(zoom: u32, x: u32, y: u32, bounds: option<tuple<f64, f64, f64, f64>>, margin: option<f64>) -> result<geometry, postgis-error>;
}
interface postgis-predicates {
    use postgis-types.{geometry};
    st-is-valid-detail: func(geom: borrow<geometry>) -> tuple<bool, option<string>, option<geometry>>;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        let by_name: std::collections::HashMap<&str, &WitFunction> =
            out.iter().map(|f| (f.kebab_name.as_str(), f)).collect();
        // bbox record return
        let mb = by_name.get("st-make-box2d").expect("st-make-box2d");
        assert!(matches!(mb.ret.inner, WitType::Bbox));
        assert!(mb.ret.fallible);
        // option<u32> param continues to classify
        let bg = by_name.get("st-box-from-geohash").expect("st-box-from-geohash");
        assert!(matches!(bg.ret.inner, WitType::Bbox));
        assert!(matches!(bg.params[1].ty, WitType::Option(_)));
        // option<tuple<f64, f64, f64, f64>> param
        let te = by_name.get("st-tile-envelope").expect("st-tile-envelope");
        assert!(matches!(te.params[3].ty, WitType::Option(_)));
        if let WitType::Option(inner) = &te.params[3].ty {
            assert!(matches!(**inner, WitType::Tuple(_)));
            if let WitType::Tuple(elems) = inner.as_ref() {
                assert_eq!(elems.len(), 4);
                assert!(matches!(elems[0], WitType::F64));
            }
        }
        // tuple<bool, option<string>, option<geometry>> return
        let vd = by_name.get("st-is-valid-detail").expect("st-is-valid-detail");
        assert!(matches!(vd.ret.inner, WitType::Tuple(_)));
        if let WitType::Tuple(elems) = &vd.ret.inner {
            assert_eq!(elems.len(), 3);
            assert!(matches!(elems[0], WitType::Bool));
            assert!(matches!(elems[1], WitType::Option(_)));
            assert!(matches!(elems[2], WitType::Option(_)));
        }
    }

    #[test]
    fn parses_list_borrow_geometry() {
        let src = r#"
interface postgis-accessors {
    use postgis-types.{geometry, postgis-error};
    st-collect: func(geoms: list<borrow<geometry>>) -> result<geometry, postgis-error>;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].params[0].ty, WitType::ListGeomBorrow));
    }

    #[test]
    fn parses_multispace_between_colon_and_func_keyword() {
        // W1 regression: mobilitydb-wasm's temporal.wit right-aligns
        // the `func(` keyword after the `:` with extra spaces for
        // visual tidiness. Phase 3's original matcher hardcoded a
        // single-space (`: func(`) and silently dropped any line
        // using extra whitespace — masked ~70 functions in the
        // `tfloat-math-ops` / `bitemporal-*-ops` / etc. interfaces.
        let src = r#"
interface tfloat-math-ops {
    use types.{tfloat-sequence};
    tfloat-abs:    func(seq: tfloat-sequence) -> tfloat-sequence;
    tfloat-negate: func(seq: tfloat-sequence) -> tfloat-sequence;
    tfloat-sqrt:   func(seq: tfloat-sequence) -> tfloat-sequence;
}
interface bitemporal-residue-ops {
    bitemporal-bool-current:  func(seq: bitemporal-bool-sequence)  -> u32;
    bitemporal-int-current:   func(seq: bitemporal-int-sequence)   -> u32;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        let names: Vec<&str> = out.iter().map(|f| f.kebab_name.as_str()).collect();
        assert!(names.contains(&"tfloat-abs"), "got {names:?}");
        assert!(names.contains(&"tfloat-negate"), "got {names:?}");
        assert!(names.contains(&"tfloat-sqrt"), "got {names:?}");
        assert!(names.contains(&"bitemporal-bool-current"), "got {names:?}");
        assert!(names.contains(&"bitemporal-int-current"), "got {names:?}");
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn parses_resource_constructor_as_synthetic_function() {
        // #556 (W3.1 mop-up): a `constructor(args)` line inside a
        // resource block becomes a WitFunction with synthesised
        // kebab `create-<resource>`, `is_constructor = true`, and
        // a return shape equal to the resource itself.
        let src = r#"
package postgis:wasm@0.1.0;
interface postgis-topology-types {
    resource topology {
        constructor(name: string, srid: s32, precision: f64);
        node-count: func() -> u32;
        to-bytes: func() -> list<u8>;
    }
    from-bytes: func(b: list<u8>) -> result<topology, topology-error>;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        let by_kebab: std::collections::HashMap<&str, &WitFunction> =
            out.iter().map(|f| (f.kebab_name.as_str(), f)).collect();

        // Constructor: kebab synthesised + flagged.
        let ctor = by_kebab
            .get("create-topology")
            .expect("constructor synthesised as create-topology");
        assert!(ctor.is_constructor);
        assert_eq!(ctor.resource.as_deref(), Some("topology"));
        assert_eq!(ctor.params.len(), 3);
        assert!(matches!(ctor.params[0].ty, WitType::String));
        assert!(matches!(ctor.params[1].ty, WitType::S32));
        assert!(matches!(ctor.params[2].ty, WitType::F64));
        // Implicit `Self` return → topology resource.
        assert!(matches!(
            ctor.ret.inner,
            WitType::Topology { borrowed: false }
        ));
        assert!(!ctor.ret.fallible);

        // Sibling instance method continues to parse with
        // `is_constructor = false` and `resource = Some(...)`.
        let nc = by_kebab.get("node-count").expect("node-count method");
        assert!(!nc.is_constructor);
        assert_eq!(nc.resource.as_deref(), Some("topology"));

        // The interface-level free function survives with
        // `resource = None` and `is_constructor = false`.
        let fb = by_kebab.get("from-bytes").expect("from-bytes free fn");
        assert!(fb.resource.is_none());
        assert!(!fb.is_constructor);
    }

    #[test]
    fn preserves_error_type_on_fallible_returns() {
        // #565 (#557fix.2): parse_return_body used to discard the
        // error half of `result<T, E>`; the reachability walker
        // needs it so postgis-error counts as live.
        let src = r#"
package postgis:wasm@0.1.0;
interface postgis-measurements {
    use postgis-types.{geometry, postgis-error};
    st-area: func(geom: borrow<geometry>) -> result<f64, postgis-error>;
    st-srid: func(geom: borrow<geometry>) -> s32;
}
"#;
        let mut out = Vec::new();
        parse_text(src, &mut out);
        let by_name: std::collections::HashMap<&str, &WitFunction> =
            out.iter().map(|f| (f.kebab_name.as_str(), f)).collect();
        let area = by_name.get("st-area").expect("st-area");
        assert!(area.ret.fallible);
        match &area.ret.error_ty {
            Some(WitType::Unsupported(name)) => assert_eq!(name, "postgis-error"),
            other => panic!("expected error_ty = Unsupported(postgis-error), got {other:?}"),
        }
        let srid = by_name.get("st-srid").expect("st-srid");
        assert!(!srid.ret.fallible);
        assert!(srid.ret.error_ty.is_none());
    }

    #[test]
    fn reachable_primary_types_includes_records_used_by_functions() {
        // Direct record return: st-make-box2d -> result<bbox,
        // postgis-error>. The bbox record sits in postgis-types.
        // postgis-error is the error variant on result<...>.
        let pkg = WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["postgis-types".to_string()],
            records: vec![
                WitRecord {
                    interface: "postgis-types".to_string(),
                    kebab_name: "bbox".to_string(),
                    fields: vec![
                        ("min-x".to_string(), "f64".to_string()),
                        ("min-y".to_string(), "f64".to_string()),
                    ],
                },
                // Unused — must NOT appear in the reachable set.
                WitRecord {
                    interface: "postgis-types".to_string(),
                    kebab_name: "buffer-params".to_string(),
                    fields: vec![("quad-segs".to_string(), "option<u32>".to_string())],
                },
            ],
            resources: vec![],
            variants: vec![WitVariantDecl {
                interface: "postgis-types".to_string(),
                kebab_name: "postgis-error".to_string(),
            }],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let fns = vec![WitFunction {
            package: "postgis:wasm".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "postgis-constructors".to_string(),
            kebab_name: "st-make-box2d".to_string(),
            params: vec![],
            ret: WitRet {
                inner: WitType::Bbox,
                fallible: true,
                error_ty: Some(WitType::Unsupported("postgis-error".to_string())),
            },
            resource: None,
            is_constructor: false,
        }];
        let reached = reachable_primary_types(&fns, &[pkg], "postgis");
        assert!(
            reached.contains(&(
                "postgis:wasm".to_string(),
                "postgis-types".to_string(),
                "bbox".to_string(),
            )),
            "bbox should be reached via Bbox return type; got {reached:?}"
        );
        assert!(
            reached.contains(&(
                "postgis:wasm".to_string(),
                "postgis-types".to_string(),
                "postgis-error".to_string(),
            )),
            "postgis-error variant should be reached via result<,E>; got {reached:?}"
        );
        assert!(
            !reached.contains(&(
                "postgis:wasm".to_string(),
                "postgis-types".to_string(),
                "buffer-params".to_string(),
            )),
            "buffer-params is unused; must NOT appear; got {reached:?}"
        );
    }

    #[test]
    fn reachable_primary_types_follows_record_fields_transitively() {
        // valid-detail has a field of type `option<coord>`. If a
        // function returns valid-detail, both records should be
        // marked reachable.
        let pkg = WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["postgis-types".to_string()],
            records: vec![
                WitRecord {
                    interface: "postgis-types".to_string(),
                    kebab_name: "coord".to_string(),
                    fields: vec![
                        ("x".to_string(), "f64".to_string()),
                        ("y".to_string(), "f64".to_string()),
                    ],
                },
                WitRecord {
                    interface: "postgis-types".to_string(),
                    kebab_name: "valid-detail".to_string(),
                    fields: vec![
                        ("valid".to_string(), "bool".to_string()),
                        ("reason".to_string(), "option<string>".to_string()),
                        ("location".to_string(), "option<coord>".to_string()),
                    ],
                },
                // Reached neither directly nor transitively.
                WitRecord {
                    interface: "postgis-types".to_string(),
                    kebab_name: "extremes".to_string(),
                    fields: vec![("x-min-point".to_string(), "coord".to_string())],
                },
            ],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let fns = vec![WitFunction {
            package: "postgis:wasm".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "postgis-predicates".to_string(),
            kebab_name: "st-is-valid-detail".to_string(),
            params: vec![],
            ret: WitRet {
                inner: WitType::Unsupported("valid-detail".to_string()),
                fallible: false,
                error_ty: None,
            },
            resource: None,
            is_constructor: false,
        }];
        let reached = reachable_primary_types(&fns, &[pkg], "postgis");
        let key = |k: &str| {
            (
                "postgis:wasm".to_string(),
                "postgis-types".to_string(),
                k.to_string(),
            )
        };
        assert!(reached.contains(&key("valid-detail")));
        assert!(
            reached.contains(&key("coord")),
            "coord must be reached through valid-detail's location field; got {reached:?}"
        );
        assert!(
            !reached.contains(&key("extremes")),
            "extremes is unreferenced; must NOT appear; got {reached:?}"
        );
    }

    #[test]
    fn reachable_primary_types_includes_resource_method_returns() {
        // wit-bindgen emits types for resource methods too, so a
        // record returned by a method must count as reached.
        let pkg = WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["postgis-types".to_string()],
            records: vec![],
            resources: vec![],
            variants: vec![],
            enums: vec![WitEnumDecl {
                interface: "postgis-types".to_string(),
                kebab_name: "geometry-type".to_string(),
                cases: vec!["point".to_string()],
            }],
            flags: vec![],
            type_aliases: vec![],
        };
        let fns = vec![WitFunction {
            package: "postgis:wasm".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "postgis-types".to_string(),
            kebab_name: "geometry-type".to_string(),
            params: vec![],
            ret: WitRet {
                inner: WitType::Unsupported("geometry-type".to_string()),
                fallible: false,
                error_ty: None,
            },
            // Method on the geometry resource.
            resource: Some("geometry".to_string()),
            is_constructor: false,
        }];
        let reached = reachable_primary_types(&fns, &[pkg], "postgis");
        assert!(reached.contains(&(
            "postgis:wasm".to_string(),
            "postgis-types".to_string(),
            "geometry-type".to_string(),
        )));
    }

    #[test]
    fn reachable_primary_types_disambiguates_same_kebab_in_two_interfaces() {
        // mobilitydb's temporal package declares `record stbox3d`
        // in BOTH `stbox-ops` and `stbox3d-ops`. wit-bindgen emits
        // two distinct Rust types (one per interface module) and the
        // force-link block must reference both. The pre-fix walker
        // keyed by kebab alone and only kept the first declaration;
        // this test guards against that regression.
        let pkg = WitPackage {
            ns_name: "mobilitydb:temporal".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec!["stbox-ops".to_string(), "stbox3d-ops".to_string()],
            records: vec![
                WitRecord {
                    interface: "stbox-ops".to_string(),
                    kebab_name: "stbox3d".to_string(),
                    fields: vec![("xmin".to_string(), "f64".to_string())],
                },
                WitRecord {
                    interface: "stbox3d-ops".to_string(),
                    kebab_name: "stbox3d".to_string(),
                    fields: vec![("xmin".to_string(), "f64".to_string())],
                },
            ],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        // Each interface has a function returning its own stbox3d.
        let fns = vec![
            WitFunction {
                package: "mobilitydb:temporal".to_string(),
                package_version: "0.1.0".to_string(),
                interface: "stbox-ops".to_string(),
                kebab_name: "stbox3d-from-text".to_string(),
                params: vec![],
                ret: WitRet {
                    inner: WitType::Unsupported("stbox3d".to_string()),
                    fallible: false,
                    error_ty: None,
                },
                resource: None,
                is_constructor: false,
            },
            WitFunction {
                package: "mobilitydb:temporal".to_string(),
                package_version: "0.1.0".to_string(),
                interface: "stbox3d-ops".to_string(),
                kebab_name: "stbox3d-make".to_string(),
                params: vec![],
                ret: WitRet {
                    inner: WitType::Unsupported("stbox3d".to_string()),
                    fallible: false,
                    error_ty: None,
                },
                resource: None,
                is_constructor: false,
            },
        ];
        let reached = reachable_primary_types(&fns, &[pkg], "mobilitydb");
        let key = |iface: &str| {
            (
                "mobilitydb:temporal".to_string(),
                iface.to_string(),
                "stbox3d".to_string(),
            )
        };
        assert!(reached.contains(&key("stbox-ops")), "got {reached:?}");
        assert!(reached.contains(&key("stbox3d-ops")), "got {reached:?}");
    }

    #[test]
    fn reachable_primary_types_falls_back_cross_interface_for_use_clauses() {
        // postgis-constructors's `st-make-box2d` returns `bbox`. The
        // bbox record sits in postgis-types and the constructor
        // interface `use postgis-types.{bbox}`s it in. The parser
        // doesn't track use clauses; cross-interface fallback finds
        // the record in another interface of the same package.
        let pkg = WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec![
                "postgis-constructors".to_string(),
                "postgis-types".to_string(),
            ],
            records: vec![WitRecord {
                interface: "postgis-types".to_string(),
                kebab_name: "bbox".to_string(),
                fields: vec![],
            }],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let fns = vec![WitFunction {
            package: "postgis:wasm".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "postgis-constructors".to_string(),
            kebab_name: "st-make-box2d".to_string(),
            params: vec![],
            ret: WitRet {
                inner: WitType::Bbox,
                fallible: false,
                error_ty: None,
            },
            resource: None,
            is_constructor: false,
        }];
        let reached = reachable_primary_types(&fns, &[pkg], "postgis");
        assert!(reached.contains(&(
            "postgis:wasm".to_string(),
            "postgis-types".to_string(),
            "bbox".to_string(),
        )));
    }

    #[test]
    fn reachable_primary_types_ignores_non_primary_packages() {
        // Functions in non-primary packages don't contribute to the
        // reachable seed, and records in non-primary packages aren't
        // indexed.
        let primary_pkg = WitPackage {
            ns_name: "postgis:wasm".to_string(),
            version: "0.1.0".to_string(),
            interfaces: vec![],
            records: vec![WitRecord {
                interface: "postgis-types".to_string(),
                kebab_name: "unused-by-postgis".to_string(),
                fields: vec![],
            }],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let other_pkg = WitPackage {
            ns_name: "sfcgal:component".to_string(),
            version: "1.0.0".to_string(),
            interfaces: vec![],
            records: vec![WitRecord {
                interface: "geometry".to_string(),
                kebab_name: "sfcgal-record".to_string(),
                fields: vec![],
            }],
            resources: vec![],
            variants: vec![],
            enums: vec![],
            flags: vec![],
            type_aliases: vec![],
        };
        let fns = vec![WitFunction {
            package: "sfcgal:component".to_string(),
            package_version: "1.0.0".to_string(),
            interface: "geometry".to_string(),
            kebab_name: "doit".to_string(),
            params: vec![],
            ret: WitRet {
                inner: WitType::Unsupported("unused-by-postgis".to_string()),
                fallible: false,
                error_ty: None,
            },
            resource: None,
            is_constructor: false,
        }];
        let reached =
            reachable_primary_types(&fns, &[primary_pkg, other_pkg], "postgis");
        // sfcgal's function references unused-by-postgis but the
        // seed walker only counts primary functions, so the type is
        // not marked reachable.
        assert!(
            reached.is_empty(),
            "non-primary functions should not seed reachability; got {reached:?}"
        );
    }
}
