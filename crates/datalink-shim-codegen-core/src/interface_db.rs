//! Dispatch IR + classifiers — the database-agnostic "what does
//! this WIT function look like at the SQL surface" layer.
//!
//! Lifted from `wasm_target/dispatch.rs` as part of step 1 of
//! PLAN-shim-codegen-datalink-migration. Step 2 moves this module
//! into the `datalink-shim-codegen-core` crate verbatim; the
//! per-database emit subtrees consume `DispatchEntry` /
//! `AggregateEntry` / `UdtfEntry` (and the inner shape enums) and
//! render the SqlValue marshalling around them.
//!
//! The module's "interface DB" name is a historical reference:
//! the per-shim interface SQLite databases produced by
//! `postgis-shim-interface` / `mobilitydb-shim-interface` are the
//! input to the classifiers, and the dispatch IR is what falls out
//! of pairing each interface-DB row against the WIT-side
//! `WitFunction` registry built by `core::wit_parse`.
//!
//! Nothing in this module touches `SqlValue` — that's the
//! emit-side boundary.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::name_match::{
    aggregate_name_candidates, collect_package_aliases, collect_package_enums,
    find_resource_method, find_same_interface_free_fn, find_wit_fn,
    index_resource_interfaces, index_resource_methods, index_wit_fns,
    index_wit_fns_nohyphen, resolve_function_aliases, sql_name_candidates,
    table_fn_name_candidates, AGGREGATE_NAME_SUFFIXES, EnumWithPackage,
};
use crate::override_tables::{override_for, tuple_pick_override_for};
use crate::record_registry::RecordType;
use crate::wit_parse::{
    self, WitEnumDecl, WitFunction, WitParam, WitRet, WitType, WitTypeAlias,
};
use shim_bridge_codegen_core::{BridgePlan, ScalarFn};

/// One dispatch arm the emitter will write into `lib.rs`.
pub struct DispatchEntry {
    /// SQL-side name as the interface DB has it (canonical or
    /// alias). The emitter reuses this to compute the func-id
    /// the match arm fires on.
    pub sql_name: String,
    /// What the arm body does. Determines argument unpacking +
    /// result wrapping.
    pub shape: DispatchShape,
}

/// Generalised scalar dispatch shape.
///
/// Each variant names the imported WIT module + function and
/// fully describes the marshaling shape (param-by-param input,
/// result wrapping). The emitter walks each entry and writes a
/// match arm of the matching shape.
pub struct DispatchShape {
    /// Rust binding-module alias (e.g. `pg_ctor`). Resolved by
    /// `wit_parse::interface_to_rust_alias`. Phase D: computed
    /// (postgis overrides + algorithmic kebab→snake fallback), so
    /// owned `String` rather than the previous `&'static str`.
    pub wit_module: String,
    /// Owning WIT package, e.g. `postgis:wasm` or `mobilitydb:temporal`.
    /// Drives the `use bindings::<ns>::<name>::<module>` line emit
    /// in `emit_lib.rs`. Phase D.
    pub wit_package: String,
    /// Snake_case function name on the binding-module side
    /// (e.g. `st_geom_from_text`).
    pub wit_func: String,
    /// Marshaling pattern for each parameter, in order.
    pub params: Vec<ParamShape>,
    /// Return-marshaling pattern.
    pub ret: RetShape,
    /// #547 (W3.1): when `Some`, this is a method on a WIT resource.
    /// `emit_arm_body` constructs the call as
    /// `arg{0}.{method_snake}({other_args})` instead of
    /// `{wit_module}::{wit_func}({all_args})`. The first
    /// `ParamShape` MUST decode into an owned resource handle
    /// (Topology / Raster) — the dispatcher rebuilds the receiver
    /// from the blob via the existing `from_topology_bytes` /
    /// `from_raster_binary` helpers.
    pub method_call: Option<MethodCall>,
}

/// #547 (W3.1): receiver context for a resource-method dispatch arm.
#[derive(Debug, Clone)]
pub struct MethodCall {
    /// Kebab-case resource name (e.g. `topology`). Used by
    /// constructor dispatch to compute the upstream Rust type ident
    /// (`Topology`); diagnostic-only for instance-method dispatch.
    pub resource_kebab: String,
    /// #556 (W3.1 mop-up): when `true`, this is a CONSTRUCTOR — the
    /// call form is `<Pascal>::new(args)` and there is NO receiver
    /// in the arg list. When `false`, this is a regular instance
    /// method: arg 0 is the owned receiver decoded via
    /// `from_*_bytes` and the call form is `arg0.{method}(rest)`.
    pub is_constructor: bool,
}

#[derive(Debug, Clone)]
pub enum ParamShape {
    /// `arg_text(&args, i, name)?`
    Text,
    /// `arg_f64(&args, i, name)?`
    F64,
    /// `arg_i64(&args, i, name)? as i32` (s32 in the WIT).
    S32,
    /// `arg_i64(&args, i, name)?` (s64 in the WIT).
    S64,
    /// `arg_i64(&args, i, name)? as u32`
    U32,
    /// `arg_i64(&args, i, name)? as u64`
    U64,
    /// `arg_i64(&args, i, name)? != 0` (bool in the WIT).
    Bool,
    /// `arg_blob(&args, i, name)?` — raw bytes.
    Blob,
    /// `from_wkb(arg_blob(...), name)?` — geometry resource.
    Geom,
    /// `Geography::from_text(arg_text(...))?` — geography resource (rare).
    Geog,
    /// `from_raster_binary(arg_blob(...), name)?` — raster resource.
    /// Round-490: postgis raster shim. The interface DB declares
    /// `binary` for raster args; the dispatch arm reconstitutes the
    /// `Raster` resource via `postgis-raster-types::from-binary` and
    /// passes a borrow to the WIT function.
    Raster,
    /// `from_topology_bytes(arg_blob(...), name)?` — topology resource.
    /// Round-490: postgis topology shim. Same pattern as Raster but
    /// routed through `postgis-topology-types::from-bytes`.
    Topology,
    /// `None` — option<T> param the codegen elects to default.
    /// The interface DB doesn't surface optional args at the
    /// SQL layer, so Phase 3 ignores the SqlValue at this index
    /// (if any) and just passes `None`.
    OptionNone,
    /// `list<borrow<geometry>>` — variadic geometry input. At the
    /// SQL layer this is exposed as the BLOB at position `start`
    /// plus every subsequent BLOB up to `start + count` (variadic
    /// scalar functions). The dispatcher decodes each into an
    /// owned `Geometry`, builds a `Vec<&Geometry>` of borrows, and
    /// passes the slice. Round 2 extension.
    ListGeom,
    /// Phase E: record-typed param decoded from a wit-value
    /// SqlValue. The bridge's serde-ops codec materialises a
    /// LOCAL record from the canon-CBOR payload, then a
    /// ciborium round-trip converts it to the UPSTREAM record
    /// type the shim function actually takes (LOCAL and UPSTREAM
    /// share field shapes by construction, so the round-trip is
    /// byte-for-byte identical except for the namespace of the
    /// generated Rust types).
    ///
    /// Carries the WIT record's kebab name + Pascal-case Rust
    /// ident + upstream-module path so emit_arm_body can
    /// reference both the LOCAL codec function and the UPSTREAM
    /// type path.
    WitValueRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
        /// True when wit-bindgen passes the upstream record by
        /// value (record is Copy). False → pass by `&`.
        upstream_by_value: bool,
    },
    /// W3.3 (#543): WIT `enum` param marshaled from `SqlValue::Integer`.
    /// SQL integer N maps to the Nth enum case in declaration order.
    /// `wit_module` is the Rust alias for the interface that
    /// declares the enum (e.g. `pg_rast_types` for `pixel-type`);
    /// `cases` is the kebab-cased case list in declaration order
    /// (PascalCase is computed at emit time).
    Enum {
        wit_module: String,
        wit_package: String,
        kebab_name: String,
        cases: Vec<String>,
    },
    /// W2 (#542): `list<X>` param over a primitive element. The SQL
    /// surface passes the list as a JSON-array literal in a TEXT
    /// argument (e.g. `'[1.0, 2.0, 3.0]'`). The dispatcher parses
    /// the text via a codegen-emitted `parse_json_list_<T>` helper
    /// and hands the resulting `Vec<T>` to the WIT function by
    /// reference. Pragmatic choice over the wit-value path: SQL
    /// users already know JSON; no per-shape codec registry is
    /// required for primitives. Complex-element lists (records,
    /// spans, geometry) still need the wit-value codec path —
    /// see plan doc W2.6 for the deferral rationale.
    ListPrim(ListPrimElem),
    /// W2 Phase 2 (#553): `list<X>` param where `X` is a record
    /// type declared in the shim's WIT (e.g. `list<int-span>`,
    /// `list<stindex-entry>`).
    ///
    /// SQL surface: JSON-array of record-shaped objects, e.g.
    /// `'[{"lower":1,"upper":10,"lower-inc":true,"upper-inc":false}, ...]'`.
    ///
    /// Dispatch arm parses the TEXT via
    /// `serde_json::from_str::<Vec<UPSTREAM>>` (wit-bindgen's
    /// `additional_derives: [Deserialize]` makes UPSTREAM records
    /// directly deserialisable; no LOCAL serde-ops codec is needed
    /// because the dispatch is by `func_id` not by `type_id`).
    /// The resulting `Vec<UPSTREAM>` is passed to the WIT call as
    /// `&arg{idx}`.
    ///
    /// Mirrors the field layout of `WitValueRecord` so the
    /// emit_arm_body machinery can re-use the upstream-path lookup.
    ListRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
    },
    /// W2 Phase 2 mop-up (#555): `list<tuple<T1, T2, ...>>` param
    /// where every Ti is primitive (today: only `list<tuple<s32,
    /// s32>>` is on the surface for mobilitydb's datespanset
    /// scalars).
    ///
    /// SQL surface: JSON-array of arrays, e.g.
    /// `'[[1, 10], [20, 30]]'` for `list<tuple<s32, s32>>`.
    ///
    /// Dispatch arm parses the TEXT via
    /// `serde_json::from_str::<Vec<(T1, T2, ...)>>` — serde_json
    /// renders Rust tuples as fixed-length JSON arrays, which
    /// matches wit-bindgen's `Vec<(i32, i32)>` binding for
    /// `list<tuple<s32, s32>>`.
    ///
    /// The per-signature helper `parse_json_list_tuple_<sig>`
    /// (e.g. `parse_json_list_tuple_i32_i32`) is emitted into the
    /// bridge's prelude exactly once per unique signature, the
    /// same way per-record `parse_json_list_record_<snake>`
    /// helpers are de-duplicated.
    ListTuple { elements: Vec<ListPrimElem> },
}

impl ParamShape {
    /// W2 Phase 2 mop-up (#555): exposes the tuple-element
    /// signature when this shape is `ListTuple`, so emit_lib can
    /// de-duplicate per-signature prelude helpers across all
    /// dispatch entries.
    pub fn list_tuple_sig(&self) -> Option<&[ListPrimElem]> {
        match self {
            ParamShape::ListTuple { elements } => Some(elements.as_slice()),
            _ => None,
        }
    }
}

/// W2 (#542): primitive element kind for `ParamShape::ListPrim`.
/// Each variant maps to a concrete Rust element type plus the
/// JSON-parser helper name emitted into the bridge's lib.rs
/// prelude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ListPrimElem {
    F64,
    F32,
    S32,
    S64,
    U32,
    U64,
    U8,
    Bool,
    String,
}

impl ListPrimElem {
    /// Rust element type produced by the parse helper.
    pub fn rust_elem(self) -> &'static str {
        match self {
            ListPrimElem::F64 | ListPrimElem::F32 => "f64",
            ListPrimElem::S32 => "i32",
            ListPrimElem::S64 => "i64",
            ListPrimElem::U32 => "u32",
            ListPrimElem::U64 => "u64",
            ListPrimElem::U8 => "u8",
            ListPrimElem::Bool => "bool",
            ListPrimElem::String => "String",
        }
    }

    /// Snake-case suffix on the `parse_json_list_<suffix>` helper.
    pub fn helper_suffix(self) -> &'static str {
        match self {
            ListPrimElem::F64 | ListPrimElem::F32 => "f64",
            ListPrimElem::S32 => "i32",
            ListPrimElem::S64 => "i64",
            ListPrimElem::U32 => "u32",
            ListPrimElem::U64 => "u64",
            ListPrimElem::U8 => "u8",
            ListPrimElem::Bool => "bool",
            ListPrimElem::String => "string",
        }
    }

    /// All variants — used by `emit_lib::emit_json_list_helpers` to
    /// walk the helper set when deciding which `parse_json_list_<T>`
    /// to emit into the bridge's prelude. Currently unused: the
    /// prelude emits all helpers unconditionally (the per-element
    /// gating optimization is deferred since the dead-code attr
    /// keeps unused helpers from triggering compile warnings).
    #[allow(dead_code)]
    pub fn all() -> &'static [ListPrimElem] {
        &[
            ListPrimElem::F64,
            ListPrimElem::F32,
            ListPrimElem::S32,
            ListPrimElem::S64,
            ListPrimElem::U32,
            ListPrimElem::U64,
            ListPrimElem::U8,
            ListPrimElem::Bool,
            ListPrimElem::String,
        ]
    }
}

#[derive(Debug, Clone)]
pub enum RetShape {
    /// `Ok(SqlValue::Text(<expr>))`
    Text,
    /// `Ok(SqlValue::Real(<expr>))`
    Real,
    /// `Ok(SqlValue::Integer(<expr> as i64))`
    Int,
    /// `Ok(SqlValue::Integer(<expr> as i64))` for bool
    BoolInt,
    /// `Ok(SqlValue::Blob(<expr>.as_wkb()))` — geometry result.
    GeomBlob,
    /// `Ok(SqlValue::Blob(<expr>.as_binary()))` — raster result.
    /// Round-490: encodes a returned `Raster` resource via the
    /// resource's own `as-binary` method (parallel to
    /// `Geometry::as_wkb`).
    RasterBlob,
    /// `Ok(SqlValue::Blob(<expr>.to_bytes()))` — topology result.
    /// Round-490: encodes a returned `Topology` resource via the
    /// resource's own `to-bytes` method.
    TopologyBlob,
    /// `Ok(SqlValue::Blob(<expr>))` — raw bytes (list<u8>).
    Blob,
    /// `Ok(match <expr> { Some(v) => SqlValue::Text(v), None => SqlValue::Null })`
    /// Round 2 extension. The inner shape selects the variant
    /// wrap on the Some side.
    OptionText,
    OptionReal,
    OptionInt,
    OptionBlob,
    OptionGeomBlob,
    /// Round-490: `option<raster>` return. Some(rast) →
    /// SqlValue::Blob(rast.as_binary()); None → SqlValue::Null.
    OptionRasterBlob,
    /// Round-490: `option<topology>` return. Some(topo) →
    /// SqlValue::Blob(topo.to_bytes()); None → SqlValue::Null.
    OptionTopologyBlob,
    /// `Ok(SqlValue::Blob(<first-geom>.as_wkb()))` — first
    /// element of a returned `list<geometry>`. Used for cluster
    /// aggregates whose WIT signature returns a list of cluster
    /// GEOMETRYCOLLECTION rows; mapping the list to one SQL
    /// blob (the first cluster, or NULL if the list is empty)
    /// is the simplest scalar-compatible projection. Round 2.
    FirstGeomBlob,
    /// Round-490: first element of a returned `list<raster>`,
    /// rendered as a WKB-equivalent (the raster `as-binary` payload)
    /// blob. Parallel to `FirstGeomBlob`.
    FirstRasterBlob,
    /// Round-490: first element of a returned `list<topology>`,
    /// rendered via the topology `to-bytes` payload. Parallel to
    /// `FirstGeomBlob`.
    FirstTopologyBlob,
    /// First element of a returned `list<option<u32>>`. Used for
    /// `st_clusterdbscan`'s SQL aggregate projection: the cluster
    /// id of the first input (None → NULL). Round 2.
    FirstOptionU32Int,
    /// `bbox` record (4 f64s: min-x, min-y, max-x, max-y).
    /// Marshaled to a WKB POLYGON envelope via
    /// `pg_ctor::st_make_envelope(xmin, ymin, xmax, ymax).as_wkb()`
    /// so the interface DB's `binary` return type is honoured.
    /// Round 3. Covers `st-make-box2d`, `st-box-from-geohash`.
    BboxBlob,
    /// The specific tuple shape `tuple<bool, option<string>,
    /// option<geometry>>` returned by `st-is-valid-detail`.
    /// Formatted as a PostgreSQL composite-type text rendering
    /// `(valid, "reason", "<wkt-location>")` so the interface DB's
    /// `text` return type is honoured. Round 3.
    IsValidDetailText,
    /// Phase E: record-typed return. The bridge encodes the
    /// UPSTREAM record via a ciborium round-trip into the LOCAL
    /// serde-ops record (same canon-CBOR bytes — round-trip works
    /// because LOCAL + UPSTREAM share field shapes by
    /// construction), then ferries it back to the host as
    /// `SqlValue::WitValue { type_id, bytes, symbolic_name }`.
    WitValueRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
        /// `<package>@<version>/<interface>/<kebab>` — diagnostic
        /// symbolic name attached to the WitValue payload.
        symbolic_name: String,
        /// Hex-formatted 32-byte sha256 over the canonical record
        /// shape — the host's match key for typed-value-binding.
        type_id_hex: String,
    },
    /// Phase F (#522): `option<bool>` return. Some(true)/Some(false)
    /// → SqlValue::Integer; None → SqlValue::Null.
    OptionBoolInt,
    /// Phase F (#522): `option<record>` return. Some(rec) →
    /// `SqlValue::WitValue(...)` (encoded via the bridge's local
    /// serde-ops codec); None → `SqlValue::Null`.
    OptionWitValueRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
        symbolic_name: String,
        type_id_hex: String,
    },
    /// Phase F (#522): `list<record>` return projected to the
    /// scalar's first element. `sql-value` has no native list
    /// variant; for SCALAR-shape functions we surface the first
    /// element as wit-value (or Null if empty). Multi-row exposure
    /// remains the table-function path. Mirrors the existing
    /// `FirstGeomBlob` / `FirstOptionU32Int` precedents.
    FirstWitValueRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
        symbolic_name: String,
        type_id_hex: String,
    },
    /// Phase F (#522): `list<s64>` / `list<u32>` / `list<s32>` /
    /// `list<u64>` projected to the first integer in scalar
    /// context (Null if empty).
    FirstInt,
    /// Phase F (#522): `list<f64>` / `list<f32>` projected to
    /// the first real in scalar context (Null if empty).
    FirstReal,
    /// Phase F (#522): `list<string>` projected to the first text
    /// in scalar context (Null if empty).
    FirstText,
    /// W3.3 (#543): WIT `enum` return marshaled to `SqlValue::Integer`.
    /// The variant is matched against the enum's case list (in
    /// declaration order) to produce the discriminant index.
    Enum {
        wit_module: String,
        wit_package: String,
        kebab_name: String,
        cases: Vec<String>,
    },
    /// W3.4 (#550) + W2 Phase 2 mop-up (#555) + W3.5 (#551):
    /// nested compound return marshaled as JSON TEXT. SQL callers
    /// consume via `json_each(...)` / SQLite's JSON1 ops.
    ///
    /// Today's surface:
    ///   - `list<list<f64>>` (postgis `st_dumpvalues`) — direct
    ///     `serde_json::to_string` on `Vec<Vec<f64>>`.
    ///   - `list<tuple<s32, s32>>` (mobilitydb `datespanset_make`)
    ///     — direct `serde_json::to_string` on `Vec<(i32, i32)>`
    ///     (serde renders Rust tuples as JSON arrays).
    ///   - `list<tuple<geometry, f64>>` (postgis
    ///     `st_dumpaspolygons`, `st_pixelaspolygons`) — hand-built
    ///     JSON: each tuple becomes `[<wkb-hex>, <value>]` because
    ///     `Geometry` is a resource and can't derive
    ///     `serde::Serialize`. The WKB-hex projection matches the
    ///     existing `GeomBlob` ret shape (same `as_wkb` bytes)
    ///     plus a hex encode for JSON-string embedding.
    ///   - `tuple<X1, X2, ...>` for primitive Xi (postgis
    ///     `st_worldtorastercoord -> tuple<s32, s32>`) — direct
    ///     `serde_json::to_string` on the upstream Rust tuple.
    ///   - `option<tuple<X1, X2, ...>>` for primitive Xi
    ///     (mobilitydb `dateset_to_span`, `floatset_to_span`,
    ///     `intset_to_span`) — Some → `serde_json` on the inner
    ///     tuple; None → `SqlValue::Null`.
    ///
    /// Rationale (over CBOR-in-wit-value): SQL users already speak
    /// JSON; SQLite's `json_each` lets them unpack rows without a
    /// host-side codec. No per-shape type-id is required.
    JsonText { kind: JsonRetKind },
    /// #564: pick element `index` of a tuple-shaped return and
    /// surface it as the matching `SqlValue` primitive variant.
    ///
    /// Used for SQL accessors that share an underlying WIT function
    /// returning `tuple<X1, X2, ...>` but expose only one element at
    /// the SQL surface. Example: postgis
    /// `st_worldtorastercoordcol(rast, x, y) -> s32` and
    /// `st_worldtorastercoordrow(rast, x, y) -> s32` both route to
    /// the WIT `st-world-to-raster-coord -> tuple<s32, s32>`; the
    /// SQL functions pick element 0 / 1 respectively.
    ///
    /// Wired via `tuple_pick_overrides()` — a hand-curated SQL-name
    /// table that maps to (interface, kebab-name, index). The
    /// underlying function's params are reused verbatim; only the
    /// return shape is rewritten to this variant.
    ///
    /// `elem` selects the SqlValue variant + cast (Integer for ints
    /// / bool, Real for floats, Text for strings). Today's postgis
    /// surface only needs s32; the other element kinds are wired
    /// for symmetry so a future shim that picks from a mixed-type
    /// tuple doesn't need another return-shape variant.
    TuplePick { index: usize, elem: ListPrimElem },
}

/// W3.4 (#550) + W2 Phase 2 mop-up (#555) + W3.5 (#551): inner
/// kind of `RetShape::JsonText`. Each variant selects one of the
/// codegen-emitted "result to JSON string" helpers in
/// `emit_arm_body`.
#[derive(Debug, Clone)]
pub enum JsonRetKind {
    /// `list<list<X>>` for primitive X — direct `serde_json`.
    ListListPrim(ListPrimElem),
    /// `list<tuple<X1, X2, ...>>` for primitive Xi — direct
    /// `serde_json` (serde renders Rust tuples as JSON arrays).
    ListTuplePrim(Vec<ListPrimElem>),
    /// `list<tuple<geometry, f64>>` — hand-built JSON with each
    /// geometry rendered as WKB hex. `Geometry` is a resource so
    /// `serde_json::to_string` over `(Geometry, f64)` isn't an
    /// option.
    ListTupleGeomF64,
    /// W3.5 (#551): `tuple<X1, X2, ...>` over primitives — direct
    /// `serde_json::to_string` on the upstream Rust tuple (serde
    /// renders Rust tuples as fixed-length JSON arrays).
    TuplePrim(Vec<ListPrimElem>),
    /// W3.5 (#551): `option<tuple<X1, X2, ...>>` over primitives.
    /// Some → JSON-array text; None → SQL NULL. Same serde tuple
    /// rendering as `TuplePrim`.
    OptionTuplePrim(Vec<ListPrimElem>),
}

/// Diagnostic: a scalar the codegen wanted to wire but couldn't.
#[derive(Debug, Clone)]
pub struct UnwiredScalar {
    pub sql_name: String,
    pub reason: String,
}

/// Same as `build_full` but for aggregates. Phase 3 only
/// wires the aggregates whose WIT signature is
/// `list<borrow<geometry>>` → `result<geometry, postgis-error>` —
/// the canonical PostGIS dissolve shape (st-union-aggregate,
/// st-polygonize-aggregate, etc.).
pub fn build_aggregate_registry(
    plan: &BridgePlan,
    wit_deps_dir: &Path,
    records: &[RecordType],
) -> Result<(Vec<AggregateEntry>, Vec<UnwiredScalar>)> {
    let wit_fns = wit_parse::parse_dir(wit_deps_dir)?;
    let aliases = collect_package_aliases(wit_deps_dir);
    let enums = collect_package_enums(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);

    // Index aggregate-shaped WIT functions by snake_case name.
    // Primary index: `postgis-aggregates` (the canonical aggregate
    // interface) plus `postgis-raster-aggregates` (#548 W3.2; raster
    // mosaic aggregates live here). Fallback index: any other
    // interface whose function's first param is a borrowed-list of
    // a supported resource type — covers `postgis-accessors::st-collect`
    // and friends that are declared as scalars but semantically
    // usable as aggregates.
    let mut agg_index: HashMap<String, &WitFunction> = HashMap::new();
    for f in &wit_fns {
        if f.interface == "postgis-aggregates"
            || f.interface == "postgis-raster-aggregates"
        {
            agg_index.insert(wit_parse::kebab_to_snake(&f.kebab_name), f);
        }
    }
    let mut agg_index_fallback: HashMap<String, &WitFunction> = HashMap::new();
    for f in &wit_fns {
        if f.interface == "postgis-aggregates"
            || f.interface == "postgis-raster-aggregates"
        {
            continue;
        }
        if f.params
            .first()
            .map(|p| {
                matches!(
                    p.ty,
                    WitType::ListGeomBorrow | WitType::ListRasterBorrow,
                )
            })
            .unwrap_or(false)
        {
            agg_index_fallback
                .entry(wit_parse::kebab_to_snake(&f.kebab_name))
                .or_insert(f);
        }
    }

    let mut entries = Vec::new();
    let mut unwired = Vec::new();
    for ext in &plan.extensions {
        for ag in &ext.aggregates {
            let candidates = aggregate_name_candidates(ag);
            let mut matched = None;
            for cand in &candidates {
                if let Some(f) = agg_index.get(cand) {
                    matched = Some(*f);
                    break;
                }
            }
            if matched.is_none() {
                for cand in &candidates {
                    if let Some(f) = agg_index_fallback.get(cand) {
                        matched = Some(*f);
                        break;
                    }
                }
            }
            // W1: suffix-strip fallback for mobilitydb's `<name>_agg`
            // duplicates (the SQL aggregate slot reuses the same
            // upstream WIT function as the bare-name scalar). Only
            // applied when no direct match exists so any genuine
            // `<...>_agg` WIT function still wins.
            if matched.is_none() {
                for cand in &candidates {
                    for suf in AGGREGATE_NAME_SUFFIXES {
                        if let Some(bare) = cand.strip_suffix(suf) {
                            if let Some(f) = agg_index.get(bare) {
                                matched = Some(*f);
                                break;
                            }
                            if let Some(f) = agg_index_fallback.get(bare) {
                                matched = Some(*f);
                                break;
                            }
                        }
                    }
                    if matched.is_some() {
                        break;
                    }
                }
            }
            let Some(f) = matched else {
                unwired.push(UnwiredScalar {
                    sql_name: ag.canonical_name.clone(),
                    reason: format!(
                        "no WIT aggregate or list<borrow<geometry>>-taking function matches any of {:?}",
                        candidates
                    ),
                });
                continue;
            };
            match classify_aggregate_shape(f, records, &enums) {
                Ok(shape) => {
                    entries.push(AggregateEntry {
                        sql_name: ag.canonical_name.clone(),
                        shape: shape.clone(),
                    });
                    for alias in &ag.aliases {
                        entries.push(AggregateEntry {
                            sql_name: alias.clone(),
                            shape: shape.clone(),
                        });
                    }
                }
                Err(reason) => unwired.push(UnwiredScalar {
                    sql_name: ag.canonical_name.clone(),
                    reason,
                }),
            }
        }
    }
    Ok((entries, unwired))
}

/// One aggregate dispatch arm.
pub struct AggregateEntry {
    pub sql_name: String,
    pub shape: AggregateShape,
}

#[derive(Debug, Clone)]
pub struct AggregateShape {
    /// Rust binding alias (always `pg_agg` for Phase 3).
    pub wit_module: String,
    /// Owning WIT package, e.g. `postgis:wasm`. Phase D.
    pub wit_package: String,
    /// Snake_case binding name (e.g. `st_union_aggregate`).
    pub wit_func: String,
    /// Number of extra args after the geometry list.
    /// `st_clusterwithin(distance)` = 1, etc.
    pub extra_args: Vec<ParamShape>,
    /// Return shape — currently always GeomBlob for the wired set.
    pub ret: RetShape,
    /// #548 (W3.2): which resource type the streaming accumulator
    /// holds. Picks the `push_*_state` / `take_*_state` prelude
    /// helpers and the decode call inside `emit_aggregate_finalize_body`.
    pub accumulator_kind: AccKind,
}

/// #548 (W3.2): resource type held by the per-context aggregate
/// accumulator. Default is `Geom` (preserving the existing
/// postgis-aggregates surface). `Raster` is used by
/// `st-rast-union-aggregate` and any future
/// `list<borrow<raster>>`-taking aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccKind {
    Geom,
    Raster,
}

/// Same as `build_registry` for UDTFs (table_functions).
/// UDTFs translate to vtab dispatch — the emitter routes each
/// to a per-name handler that materialises a row set up front.
/// Phase 3 wires the row-yielding UDTFs as eager vtabs; the more
/// complex streaming forms (st_subdivide on huge inputs) are
/// noted as unwired.
pub fn build_udtf_registry(
    plan: &BridgePlan,
    wit_deps_dir: &Path,
    records: &[RecordType],
) -> Result<(Vec<UdtfEntry>, Vec<UnwiredScalar>)> {
    let wit_fns = wit_parse::parse_dir(wit_deps_dir)?;
    let aliases = collect_package_aliases(wit_deps_dir);
    let enums = collect_package_enums(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);
    let wit_index = index_wit_fns(&wit_fns);
    let wit_nohyphen = index_wit_fns_nohyphen(&wit_fns);

    let mut entries = Vec::new();
    let mut unwired = Vec::new();
    for ext in &plan.extensions {
        for tf in &ext.table_functions {
            let candidates = table_fn_name_candidates(tf);
            let matched = find_wit_fn(&candidates, &wit_index, &wit_nohyphen);
            let Some(f) = matched else {
                unwired.push(UnwiredScalar {
                    sql_name: tf.canonical_name.clone(),
                    reason: format!(
                        "no WIT function matches any of {:?} for UDTF",
                        candidates
                    ),
                });
                continue;
            };
            // UDTFs that take a single geometry and return a
            // `list<geometry>` are the cleanly-wired shape.
            // Anything else (record returns, multi-arg shapes)
            // is noted as deferred.
            match classify_udtf_shape(f, records, &aliases, &enums) {
                Ok(shape) => {
                    entries.push(UdtfEntry {
                        sql_name: tf.canonical_name.clone(),
                        shape: shape.clone(),
                    });
                    for alias in &tf.aliases {
                        entries.push(UdtfEntry {
                            sql_name: alias.clone(),
                            shape: shape.clone(),
                        });
                    }
                }
                Err(reason) => unwired.push(UnwiredScalar {
                    sql_name: tf.canonical_name.clone(),
                    reason,
                }),
            }
        }
    }
    Ok((entries, unwired))
}

pub struct UdtfEntry {
    pub sql_name: String,
    pub shape: UdtfShape,
}

#[derive(Debug, Clone)]
pub struct UdtfShape {
    pub wit_module: String,
    /// Owning WIT package, e.g. `postgis:wasm`. Phase D.
    pub wit_package: String,
    pub wit_func: String,
    pub params: Vec<ParamShape>,
    /// Whether the WIT-side call returns `result<list<geometry>,
    /// postgis-error>` (true) or bare `list<geometry>` (false).
    pub fallible: bool,
    /// Names of the WIT-side params, in source order. The codegen
    /// emits them as HIDDEN columns on the vtab's `CREATE TABLE`
    /// so the SQL-side `f(a, b)` call binds each positional argv
    /// to the matching HIDDEN column. Task #531.
    pub param_names: Vec<String>,
    /// Schema of the rows the WIT-side function emits, derived from
    /// the return type. Drives the visible-column portion of the
    /// vtab's `CREATE TABLE`. Task #531.
    pub output_row: UdtfOutputRow,
}

/// Row shape of a UDTF's return list. Driven by classify_udtf_shape
/// from the WIT-side return type; consumed by emit_vtab_impl when it
/// composes the per-vtab CREATE TABLE schema. Task #531.
#[derive(Debug, Clone)]
pub enum UdtfOutputRow {
    /// `list<geometry>` — one BLOB column. The column name is left
    /// for the emitter to pick based on the SQL function name
    /// (a `st-dump-points` row is conventionally `point`; everything
    /// else is `geom`). Phase 3's filter body already materialises
    /// these as WKB blobs.
    SingleGeom,
    /// `list<T>` for a primitive `T` — one column whose affinity is
    /// derived from `T`. The column name is `value` (the SQL surface
    /// has no better hint).
    SinglePrimitive { affinity: ColumnAffinity },
    /// `list<record-name>` where `record-name` is in the per-shim
    /// record registry. The vtab declares one column per record
    /// field; affinity is per-field. Used for mobilitydb table
    /// functions like `temporal-join-float` → `list<joined-float-pair>`.
    Record { fields: Vec<UdtfColumn> },
    /// The codegen couldn't classify the row shape (e.g. the return
    /// type is `string` directly, or a `result<...>` that wraps an
    /// unrecognised body). The emitter falls back to a single
    /// `value BLOB` column so the vtab is still loadable.
    Unwired { reason: String },
}

#[derive(Debug, Clone)]
pub struct UdtfColumn {
    /// Column name as written in the WIT (kebab-case is preserved;
    /// the emitter quotes the identifier in the CREATE TABLE so
    /// hyphens are legal).
    pub name: String,
    pub affinity: ColumnAffinity,
    /// Per-field value-extraction recipe for the row's i-th column.
    /// Used by `emit_udtf_filter_body` to turn each upstream record
    /// instance into a `Vec<SqlValue>` of column values. Task #532.
    pub field_shape: UdtfFieldShape,
}

/// Per-field value-extraction recipe — drives the codegen-emitted
/// row decomposer that turns `list<record>` UDTF returns into
/// `Vec<Vec<SqlValue>>` rows. Task #532.
#[derive(Debug, Clone)]
pub enum UdtfFieldShape {
    /// Integer affinity field: s32/s64/u32/u64/u8/bool, plus
    /// aliases that resolve to one of those (e.g. timestamp-tz).
    /// Emitted as `SqlValue::Integer(<field> as i64)`.
    Int,
    /// Real affinity field: f32/f64.
    /// Emitted as `SqlValue::Real(<field> as f64)`.
    Real,
    /// String field — `SqlValue::Text(<field>.clone())`.
    Text,
    /// `list<u8>` field — `SqlValue::Blob(<field>.clone())`.
    Blob,
    /// Geometry/geography field — `SqlValue::Blob(<field>.as_wkb())`.
    GeomBlob,
    /// `option<Int>` — Some(v) → Integer; None → Null.
    OptionInt,
    /// `option<Real>` — Some(v) → Real; None → Null.
    OptionReal,
    /// `option<string>` — Some(v) → Text; None → Null.
    OptionText,
    /// `option<list<u8>>` — Some(v) → Blob; None → Null.
    OptionBlob,
    /// `option<geometry|geography>` — Some(v) → Blob(v.as_wkb()); None → Null.
    OptionGeomBlob,
    /// Anything else (nested records, tuples, lists). The emitter
    /// substitutes `SqlValue::Null` so the row decomposer still
    /// compiles and the vtab loads. Future task: encode as wit-value.
    Unsupported,
}

/// Classify a parsed field type into a `UdtfFieldShape`. Used by
/// `classify_list_inner_row` when walking the record's fields.
/// Task #532.
pub fn field_shape_for(ty: &WitType) -> UdtfFieldShape {
    match ty {
        WitType::S32
        | WitType::S64
        | WitType::U32
        | WitType::U64
        | WitType::U8
        | WitType::Bool => UdtfFieldShape::Int,
        WitType::F32 | WitType::F64 => UdtfFieldShape::Real,
        WitType::String => UdtfFieldShape::Text,
        WitType::ListU8 => UdtfFieldShape::Blob,
        WitType::Geometry { .. } | WitType::Geography { .. } => UdtfFieldShape::GeomBlob,
        WitType::Option(inner) => match inner.as_ref() {
            WitType::S32
            | WitType::S64
            | WitType::U32
            | WitType::U64
            | WitType::U8
            | WitType::Bool => UdtfFieldShape::OptionInt,
            WitType::F32 | WitType::F64 => UdtfFieldShape::OptionReal,
            WitType::String => UdtfFieldShape::OptionText,
            WitType::ListU8 => UdtfFieldShape::OptionBlob,
            WitType::Geometry { .. } | WitType::Geography { .. } => UdtfFieldShape::OptionGeomBlob,
            _ => UdtfFieldShape::Unsupported,
        },
        _ => UdtfFieldShape::Unsupported,
    }
}

/// SQLite type-affinity strings the emitter writes into the
/// CREATE TABLE column declarations. See SQLite docs §3.1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAffinity {
    Integer,
    Real,
    Text,
    Blob,
}

impl ColumnAffinity {
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnAffinity::Integer => "INTEGER",
            ColumnAffinity::Real => "REAL",
            ColumnAffinity::Text => "TEXT",
            ColumnAffinity::Blob => "BLOB",
        }
    }
}

/// Map a WIT type to its SQLite column affinity. Used by both the
/// HIDDEN-column emission (one column per WIT param) and the
/// record-field walk (one column per field of the row record).
/// Task #531.
pub fn affinity_for(ty: &WitType) -> ColumnAffinity {
    match ty {
        WitType::S32
        | WitType::S64
        | WitType::U32
        | WitType::U64
        | WitType::U8
        | WitType::Bool => ColumnAffinity::Integer,
        WitType::F32 | WitType::F64 => ColumnAffinity::Real,
        WitType::String => ColumnAffinity::Text,
        WitType::ListU8 => ColumnAffinity::Blob,
        WitType::Geometry { .. } | WitType::Geography { .. } => ColumnAffinity::Blob,
        WitType::Raster { .. } | WitType::Topology { .. } => ColumnAffinity::Blob,
        WitType::Bbox => ColumnAffinity::Blob,
        WitType::ListGeomBorrow | WitType::ListGeomOwned => ColumnAffinity::Blob,
        WitType::ListRasterBorrow => ColumnAffinity::Blob,
        WitType::ListOptionU32 => ColumnAffinity::Text, // JSON-encoded
        WitType::List(_) => ColumnAffinity::Text, // JSON-encoded fallback
        WitType::Option(inner) => affinity_for(inner),
        WitType::Tuple(_) => ColumnAffinity::Blob, // composite → BLOB
        WitType::Result(ok, _err) => affinity_for(ok),
        WitType::Unsupported(_) => ColumnAffinity::Blob, // records → wit-value bytes
    }
}

/// Pick the visible column name for the single-column row shape
/// (`list<geometry>` returns). The default `geom` matches the
/// PostgreSQL `geometry_dump.geom` field convention; SQL names
/// whose tail is `point`/`points` get the more familiar `point`
/// so queries like `WHERE ST_X(point) > 0` stay literate.
pub fn single_geom_column_name_for(sql_name: &str) -> &'static str {
    let lower = sql_name.to_ascii_lowercase();
    if lower.ends_with("point") || lower.ends_with("points") {
        "point"
    } else {
        "geom"
    }
}

/// Public — used by emit_vtab_impl when emitting the CREATE TABLE
/// for a `SingleGeom` row shape.
pub fn udtf_single_geom_column_name(sql_name: &str) -> &'static str {
    single_geom_column_name_for(sql_name)
}


/// Map a WIT function's parsed signature to a `DispatchShape`,
/// or return an error string describing why the codegen can't
/// wire it.
pub fn classify_shape(
    f: &WitFunction,
    records: &[RecordType],
    enums: &[EnumWithPackage],
) -> Result<DispatchShape, String> {
    let alias = wit_parse::interface_to_rust_alias(&f.interface).ok_or_else(|| {
        format!(
            "WIT interface '{}' has no Rust-binding alias mapping yet",
            f.interface
        )
    })?;
    let wit_func = wit_parse::kebab_to_snake(&f.kebab_name);

    // #547 (W3.1): for resource methods, the WIT signature lists
    // method-only params (no implicit `self`). At the SQL-call layer
    // the receiver gets prepended as arg 0 (the blob of the resource);
    // classify it as ParamShape::Topology / Raster here so the
    // dispatcher decodes via `from_topology_bytes` / `from_raster_binary`.
    // #556 (W3.1 mop-up): constructors are method-shaped in the WIT
    // (they live inside the resource block) but they take no receiver
    // — all listed params are real args and the call form is
    // `<Pascal>::new(args)`. classify_param walks the listed params
    // directly without prepending a receiver shape; classify_return
    // sees `topology`/`raster` and produces the matching blob
    // encoder ret shape.
    let mut params = Vec::with_capacity(f.params.len() + 1);
    let method_call = if let Some(ref rkebab) = f.resource {
        if !f.is_constructor {
            let recv_shape = match rkebab.as_str() {
                "topology" => ParamShape::Topology,
                "raster" => ParamShape::Raster,
                // Other resources can be added as their `from-bytes`
                // helpers land. Bail clearly so the unwired-symbol
                // diagnostic surfaces the gap.
                other => {
                    return Err(format!(
                        "resource-method receiver `{}` has no blob decoder wired",
                        other,
                    ));
                }
            };
            params.push(recv_shape);
        }
        Some(MethodCall {
            resource_kebab: rkebab.clone(),
            is_constructor: f.is_constructor,
        })
    } else {
        None
    };

    // Classify each parameter.
    for (i, p) in f.params.iter().enumerate() {
        let ps = classify_param(&p.ty, records, enums).map_err(|why| {
            format!(
                "param #{i} ({:?}: {:?}) not wired: {why}",
                p.name, p.ty
            )
        })?;
        params.push(ps);
    }

    let ret = classify_return(&f.ret, records, enums)?;

    Ok(DispatchShape {
        wit_module: alias,
        wit_package: f.package.clone(),
        wit_func,
        params,
        ret,
        method_call,
    })
}

pub fn classify_aggregate_shape(
    f: &WitFunction,
    records: &[RecordType],
    enums: &[EnumWithPackage],
) -> Result<AggregateShape, String> {
    // Round 2: aggregates may live in any interface that takes
    // `list<borrow<geometry>>` as the first arg — not just
    // postgis-aggregates. The wit_module alias is whatever the
    // interface mapper says.
    let alias = wit_parse::interface_to_rust_alias(&f.interface).ok_or_else(|| {
        format!(
            "aggregate interface '{}' has no Rust-binding alias mapping",
            f.interface
        )
    })?;
    let wit_func = wit_parse::kebab_to_snake(&f.kebab_name);

    // First param must be a borrowed-list of a supported resource
    // type. #548 (W3.2): both `list<borrow<geometry>>` (Geom) and
    // `list<borrow<raster>>` (Raster) are accepted; the kind picks
    // the per-context accumulator helpers + the finalize decoder.
    if f.params.is_empty() {
        return Err("aggregate has zero params".into());
    }
    let first = &f.params[0].ty;
    let accumulator_kind = match first {
        WitType::ListGeomBorrow => AccKind::Geom,
        WitType::ListRasterBorrow => AccKind::Raster,
        other => {
            return Err(format!(
                "first aggregate param must be list<borrow<geometry>> or list<borrow<raster>>; got {:?}",
                other,
            ));
        }
    };

    // Subsequent params (st-cluster-within takes f64 distance, etc.)
    let mut extra = Vec::new();
    for (i, p) in f.params.iter().enumerate().skip(1) {
        extra.push(classify_param(&p.ty, records, enums).map_err(|why| {
            format!("aggregate extra param #{i}: {why}")
        })?);
    }

    let ret = classify_return(&f.ret, records, enums)?;
    Ok(AggregateShape {
        wit_module: alias,
        wit_package: f.package.clone(),
        wit_func,
        extra_args: extra,
        ret,
        accumulator_kind,
    })
}

pub fn classify_udtf_shape(
    f: &WitFunction,
    records: &[RecordType],
    aliases: &[WitTypeAlias],
    enums: &[EnumWithPackage],
) -> Result<UdtfShape, String> {
    let alias = wit_parse::interface_to_rust_alias(&f.interface).ok_or_else(|| {
        format!(
            "UDTF interface '{}' has no Rust-binding alias mapping",
            f.interface
        )
    })?;
    let wit_func = wit_parse::kebab_to_snake(&f.kebab_name);
    let mut params = Vec::with_capacity(f.params.len());
    let mut param_names = Vec::with_capacity(f.params.len());
    for (i, p) in f.params.iter().enumerate() {
        params.push(classify_param(&p.ty, records, enums).map_err(|why| {
            format!("UDTF param #{i}: {why}")
        })?);
        // Fall back to `arg{N}` if the WIT didn't carry a name
        // (defensive — the parser populates names for every
        // declared param).
        let name = if p.name.is_empty() {
            format!("arg{i}")
        } else {
            p.name.clone()
        };
        param_names.push(name);
    }
    let output_row = classify_udtf_output_row(&f.ret.inner, records, aliases);
    Ok(UdtfShape {
        wit_module: alias,
        wit_package: f.package.clone(),
        wit_func,
        params,
        fallible: f.ret.fallible,
        param_names,
        output_row,
    })
}

/// Inspect the return type and produce a row-shape descriptor for
/// the vtab's visible columns. Task #531.
pub fn classify_udtf_output_row(
    t: &WitType,
    records: &[RecordType],
    aliases: &[WitTypeAlias],
) -> UdtfOutputRow {
    match t {
        WitType::ListGeomOwned => UdtfOutputRow::SingleGeom,
        WitType::List(inner) => classify_list_inner_row(inner, records, aliases),
        WitType::ListU8 => UdtfOutputRow::Unwired {
            reason: "row shape `list<u8>` is a single blob, not a row list".to_string(),
        },
        WitType::ListOptionU32 => UdtfOutputRow::SinglePrimitive {
            affinity: ColumnAffinity::Integer,
        },
        WitType::ListGeomBorrow => UdtfOutputRow::Unwired {
            reason: "row shape `list<borrow<geometry>>` not valid as a return".to_string(),
        },
        WitType::ListRasterBorrow => UdtfOutputRow::Unwired {
            reason: "row shape `list<borrow<raster>>` not valid as a return".to_string(),
        },
        // `parse_type` collapses `list<<Unsupported>>` into the
        // bare string form `Unsupported("list<X>")` so diagnostics
        // for scalar returns stay specific. For UDTFs we still
        // need to recognise the row shape, so we unwrap the
        // collapsed form here when it starts with `list<`. Task #531.
        WitType::Unsupported(s) if s.starts_with("list<") && s.ends_with('>') => {
            let inner = &s["list<".len()..s.len() - 1];
            // Re-parse the inner so nested compound shapes
            // (e.g. `list<option<...>>`) still classify through
            // the regular alphabet.
            let inner_ty = crate::wit_parse::parse_type_public(inner);
            classify_list_inner_row(&inner_ty, records, aliases)
        }
        other => UdtfOutputRow::Unwired {
            reason: format!(
                "row shape not in vtab-schema alphabet: {} (expected list<...>)",
                type_label_dbg(other)
            ),
        },
    }
}

/// `list<T>` inner classification used by `classify_udtf_output_row`.
pub fn classify_list_inner_row(
    inner: &WitType,
    records: &[RecordType],
    aliases: &[WitTypeAlias],
) -> UdtfOutputRow {
    match inner {
        WitType::Geometry { .. } | WitType::Geography { .. } => UdtfOutputRow::SingleGeom,
        WitType::Unsupported(name) => {
            if let Some(rec) = records.iter().find(|r| &r.kebab_name == name) {
                let fields = rec
                    .fields
                    .iter()
                    .map(|(fname, ftype_text)| {
                        // The raw type-text in the record registry is
                        // the literal WIT body (e.g. "s64", "list<u8>",
                        // "timestamp-tz"); we parse it through the
                        // same alphabet the dispatcher uses then
                        // apply the package's alias table (so e.g.
                        // `timestamp-tz` resolves to `s64`) before
                        // mapping to a SQLite affinity + value
                        // extraction shape. Task #532.
                        let parsed = crate::wit_parse::parse_type_public(ftype_text);
                        let resolved = crate::wit_parse::resolve_aliases(parsed, aliases);
                        UdtfColumn {
                            name: fname.clone(),
                            affinity: affinity_for(&resolved),
                            field_shape: field_shape_for(&resolved),
                        }
                    })
                    .collect();
                UdtfOutputRow::Record { fields }
            } else {
                UdtfOutputRow::Unwired {
                    reason: format!(
                        "row record `{name}` not in shim record registry"
                    ),
                }
            }
        }
        WitType::S32 | WitType::S64 | WitType::U32 | WitType::U64 | WitType::U8
        | WitType::Bool => UdtfOutputRow::SinglePrimitive {
            affinity: ColumnAffinity::Integer,
        },
        WitType::F32 | WitType::F64 => UdtfOutputRow::SinglePrimitive {
            affinity: ColumnAffinity::Real,
        },
        WitType::String => UdtfOutputRow::SinglePrimitive {
            affinity: ColumnAffinity::Text,
        },
        WitType::ListU8 => UdtfOutputRow::SinglePrimitive {
            affinity: ColumnAffinity::Blob,
        },
        other => UdtfOutputRow::Unwired {
            reason: format!(
                "row element shape not in vtab-schema alphabet: {}",
                type_label_dbg(other)
            ),
        },
    }
}

/// Phase C C3 hook — when an `Unsupported(name)` param type lines up
/// with a record declared in the per-shim `record_registry`, the
/// codegen WOULD route the arm through `SqlValue::WitValue` (decode
/// via `<record>_from_canon_cbor(payload.bytes)`, encode via the
/// matching `_to_canon_cbor`).  For the current shim corpus this
/// path is unreachable:
///
///   - postgis: no scalar's WIT signature takes a record. The 18
///     records in postgis-types / postgis-aggregates / etc. are
///     either return types (covered by `BboxBlob` /
///     `IsValidDetailText` projections) or shape-internal (used
///     only inside the wasm-side function body, never crossing
///     the SQL boundary).
///   - mobilitydb: the temporal WIT (which DOES have record params)
///     isn't in the resolved deps tree yet — Phase E lands a
///     proper deps root that includes
///     `crates/mdb-temporal-wasm/wit/`.
///
/// So the C3 dispatcher emit is a no-op against today's inputs.
/// The Phase E follow-up is `dispatch::classify_param` taking a
/// `&[RecordType]` so an `Unsupported(name)` that matches a
/// record's `kebab_name` routes to a `ParamShape::WitValueRecord`
/// (new variant) plus the matching `RetShape::WitValueRecord`.
/// `emit_arm_body` then emits the decode/encode shape per the
/// PLAN doc:
///
///   N => {
///       let arg0 = match args.get(0) {
///           Some(SqlValue::WitValue(p)) => {
///               <record>_from_canon_cbor(p.bytes.clone()) ...
///           }
///           _ => return Err(...)
///       };
///       let r = <module>::<func>(arg0);
///       Ok(SqlValue::WitValue(WitValuePayload {
///           type_id: <hash bytes>.to_vec(),
///           bytes: <record>_to_canon_cbor(r),
///           symbolic_name: "<symbolic>".into(),
///       }))
///   }
pub fn classify_param(
    t: &WitType,
    records: &[RecordType],
    enums: &[EnumWithPackage],
) -> Result<ParamShape, String> {
    Ok(match t {
        WitType::Geometry { .. } => ParamShape::Geom,
        WitType::Geography { .. } => ParamShape::Geog,
        WitType::Raster { .. } => ParamShape::Raster,
        WitType::Topology { .. } => ParamShape::Topology,
        WitType::String => ParamShape::Text,
        WitType::F64 => ParamShape::F64,
        WitType::F32 => ParamShape::F64, // SqlValue widens to f64
        WitType::S32 => ParamShape::S32,
        WitType::S64 => ParamShape::S64,
        WitType::U32 => ParamShape::U32,
        WitType::U64 => ParamShape::U64,
        WitType::U8 => ParamShape::U32, // promoted
        WitType::Bool => ParamShape::Bool,
        WitType::ListU8 => ParamShape::Blob,
        WitType::ListGeomBorrow => ParamShape::ListGeom,
        WitType::ListRasterBorrow => {
            // #548 (W3.2): `list<borrow<raster>>` only appears as the
            // first arg of a raster aggregate; classify_aggregate_shape
            // handles it directly and sets AccKind::Raster. Bare-
            // scalar use isn't on the postgis surface today.
            return Err(format!(
                "param type not in dispatcher alphabet: list<borrow<raster>> (aggregate-only)"
            ));
        }
        WitType::ListGeomOwned => {
            return Err(format!(
                "param type not in dispatcher alphabet: list<geometry> (owned; only returns are supported)"
            ));
        }
        WitType::ListOptionU32 => {
            return Err(format!(
                "param type not in dispatcher alphabet: list<option<u32>> (returns only)"
            ));
        }
        // Round 3: `option<tuple<...>>` (and any other option) → None
        // is the Phase 3 default — the SQL surface doesn't expose
        // optional args, so dispatching with `None` matches the
        // "use the function's defaults" convention. Covers
        // `st-tile-envelope`'s `bounds` / `margin` args.
        WitType::Option(_) => ParamShape::OptionNone,
        WitType::Tuple(_) => {
            // Bare tuple params don't appear in PostGIS today. Bail
            // explicitly so the round-3 work item is named if one
            // ever shows up.
            return Err(format!(
                "param type not in dispatcher alphabet: tuple<...> (only option<tuple<...>> is wired)"
            ));
        }
        WitType::Bbox => {
            // Bare bbox params (not wrapped in option) likewise
            // don't appear in the postgis-wasm surface today.
            return Err(format!(
                "param type not in dispatcher alphabet: bbox (returns only)"
            ));
        }
        WitType::List(inner) => {
            // W2 Phase 1 (#542): primitive-element `list<X>` param
            // via JSON-as-TEXT marshaling. SQL passes a JSON array
            // literal (`'[1.0, 2.0]'`); the dispatch arm parses it
            // into a `Vec<X>` via a codegen-emitted helper and
            // hands it to the WIT function by reference.
            if let Some(elem) = list_prim_elem(inner) {
                return Ok(ParamShape::ListPrim(elem));
            }
            // W2 Phase 2 (#553): record-element `list<X>` param.
            // SQL passes a JSON array of record-shaped objects;
            // the dispatch arm decodes into a `Vec<UPSTREAM>` via
            // `serde_json::from_str` against the wit-bindgen-
            // generated UPSTREAM record (which carries serde
            // derives via the bindgen invocation's
            // additional_derives).
            if let WitType::Unsupported(name) = inner.as_ref() {
                if let Some(rec) = records.iter().find(|r| &r.kebab_name == name) {
                    return Ok(ParamShape::ListRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                    });
                }
            }
            // W2 Phase 2 mop-up (#555): primitive-element tuple
            // list, e.g. `list<tuple<s32, s32>>` for mobilitydb's
            // datespanset scalars. Each tuple-element must be a
            // primitive recognised by `list_prim_elem`; we fall
            // through to the generic Err otherwise so the
            // diagnostic names the unwired shape.
            if let WitType::Tuple(elems) = inner.as_ref() {
                let prims: Option<Vec<ListPrimElem>> =
                    elems.iter().map(list_prim_elem).collect();
                if let Some(prims) = prims {
                    if !prims.is_empty() {
                        return Ok(ParamShape::ListTuple { elements: prims });
                    }
                }
            }
            return Err(format!(
                "param type not in dispatcher alphabet: list<{}> (element shape not yet wired; record elements need a matching kebab in the registry)",
                type_label_dbg(inner),
            ));
        }
        WitType::Result(ok, _) => {
            return Err(format!(
                "param type not in dispatcher alphabet: result<{}> (result is not a parameter shape)",
                type_label_dbg(ok),
            ));
        }
        WitType::Unsupported(s) => {
            // W3.3 (#543): WIT enums surface here because parse_type
            // has no Enum variant — the bare kebab name (e.g.
            // `pixel-type`) falls through. Check the enum registry
            // BEFORE records so an enum-named-the-same-as-a-record
            // (none in practice today) would still take this branch.
            if let Some(en) = enums.iter().find(|e| &e.decl.kebab_name == s) {
                let wit_module =
                    wit_parse::interface_to_rust_alias(&en.decl.interface).ok_or_else(|| {
                        format!(
                            "enum '{}' lives in interface '{}' with no Rust-binding alias",
                            s, en.decl.interface,
                        )
                    })?;
                return Ok(ParamShape::Enum {
                    wit_module,
                    wit_package: en.package.clone(),
                    kebab_name: en.decl.kebab_name.clone(),
                    cases: en.decl.cases.clone(),
                });
            }
            // Phase E: record-typed params route through wit-value.
            // The unsupported name might match a record's kebab —
            // if so, the dispatch arm decodes via the bridge's local
            // serde-ops codec, ciborium round-trips to the upstream
            // record shape, and passes to the WIT function. Both
            // ends are structurally identical so the round-trip is
            // a strict re-encode/re-decode against the same byte
            // shape.
            if let Some(rec) = records.iter().find(|r| &r.kebab_name == s) {
                return Ok(ParamShape::WitValueRecord {
                    kebab_name: rec.kebab_name.clone(),
                    wit_interface: rec.interface.clone(),
                    wit_package: rec.package.clone(),
                    wit_package_version: rec.package_version.clone(),
                    upstream_by_value: rec.is_copy,
                });
            }
            return Err(format!("param type not in dispatcher alphabet: {s}"));
        }
    })
}

pub fn classify_return(
    r: &WitRet,
    records: &[RecordType],
    enums: &[EnumWithPackage],
) -> Result<RetShape, String> {
    Ok(match &r.inner {
        WitType::Geometry { .. } => RetShape::GeomBlob,
        WitType::Geography { .. } => RetShape::GeomBlob,
        WitType::Raster { .. } => RetShape::RasterBlob,
        WitType::Topology { .. } => RetShape::TopologyBlob,
        WitType::String => RetShape::Text,
        WitType::F64 | WitType::F32 => RetShape::Real,
        WitType::S32 | WitType::S64 | WitType::U32 | WitType::U64 | WitType::U8 => {
            RetShape::Int
        }
        WitType::Bool => RetShape::BoolInt,
        WitType::ListU8 => RetShape::Blob,
        WitType::ListGeomOwned => RetShape::FirstGeomBlob,
        WitType::ListOptionU32 => RetShape::FirstOptionU32Int,
        WitType::Option(inner) => match inner.as_ref() {
            // Round 2 extension: option<T> return is unwrapped to
            // SqlValue::Null on the None side; Some(v) is wrapped
            // per the inner shape's variant.
            WitType::String => RetShape::OptionText,
            WitType::F64 | WitType::F32 => RetShape::OptionReal,
            WitType::S32
            | WitType::S64
            | WitType::U32
            | WitType::U64
            | WitType::U8 => RetShape::OptionInt,
            // Phase F (#522): option<bool> previously fell through
            // to "not supported". Mobilitydb has 4 such returns.
            WitType::Bool => RetShape::OptionBoolInt,
            WitType::ListU8 => RetShape::OptionBlob,
            WitType::Geometry { .. } | WitType::Geography { .. } => RetShape::OptionGeomBlob,
            // Round-490: option<raster> — Some(rast) → Blob via
            // `as-binary`; None → Null.
            WitType::Raster { .. } => RetShape::OptionRasterBlob,
            // Round-490: option<topology> — Some(topo) → Blob via
            // `to-bytes`; None → Null.
            WitType::Topology { .. } => RetShape::OptionTopologyBlob,
            // W3.5 (#551): option<tuple<X1, X2, ...>> over primitive
            // Xi — Some(t) → JSON-array text via serde; None → Null.
            // Covers mobilitydb `dateset_to_span`, `floatset_to_span`,
            // `intset_to_span` (all `option<tuple<X, X>>`).
            WitType::Tuple(elems) => {
                let prims: Option<Vec<ListPrimElem>> =
                    elems.iter().map(list_prim_elem).collect();
                if let Some(prims) = prims {
                    if !prims.is_empty() {
                        return Ok(RetShape::JsonText {
                            kind: JsonRetKind::OptionTuplePrim(prims),
                        });
                    }
                }
                let parts: Vec<String> = elems.iter().map(type_label_dbg).collect();
                return Err(format!(
                    "return type not in dispatcher alphabet: option<tuple<{}>> (tuple shape not yet wired)",
                    parts.join(", ")
                ));
            }
            // Phase F (#522): option<record>. Inner unsupported(name)
            // hits the record registry; if found, route to
            // `OptionWitValueRecord` — Some(rec)→wit-value,
            // None→Null.
            WitType::Unsupported(s) => {
                if let Some(rec) = records.iter().find(|r| &r.kebab_name == s) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    return Ok(RetShape::OptionWitValueRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                    });
                }
                return Err(format!(
                    "return type not in dispatcher alphabet: option<{}> (no matching record in shim registry)",
                    s,
                ));
            }
            other => {
                return Err(format!(
                    "return type not in dispatcher alphabet: option<{}> (inner not yet supported)",
                    type_label_dbg(other)
                ));
            }
        },
        // Phase F (#522): generic list return projected to its
        // first element in scalar context (the contract has no
        // list variant on `sql-value`). Records → wit-value, prims
        // → first integer / real / text. Multi-row exposure stays
        // on the table-function path.
        WitType::List(inner) => match inner.as_ref() {
            WitType::Unsupported(s) => {
                if let Some(rec) = records.iter().find(|r| &r.kebab_name == s) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    return Ok(RetShape::FirstWitValueRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                    });
                }
                return Err(format!(
                    "return type not in dispatcher alphabet: list<{}> (no matching record in shim registry)",
                    s,
                ));
            }
            WitType::S32 | WitType::S64 | WitType::U32 | WitType::U64 | WitType::U8 => {
                RetShape::FirstInt
            }
            WitType::F64 | WitType::F32 => RetShape::FirstReal,
            WitType::String => RetShape::FirstText,
            // Round-490: list<raster> — first element rendered as
            // BLOB via the resource's `as-binary` method (Null if
            // empty). Mirrors `FirstGeomBlob`.
            WitType::Raster { .. } => RetShape::FirstRasterBlob,
            // Round-490: list<topology> — first element rendered via
            // the resource's `to-bytes` method (Null if empty).
            WitType::Topology { .. } => RetShape::FirstTopologyBlob,
            // W3.4 (#550): `list<list<X>>` — JSON-encode as TEXT.
            // Used by postgis `st_dumpvalues -> list<list<f64>>`.
            // Only primitive inner element types are wired today;
            // a deeper nest (`list<list<record>>`) would need its
            // own helper shape.
            WitType::List(inner2) => {
                if let Some(prim) = list_prim_elem(inner2) {
                    return Ok(RetShape::JsonText {
                        kind: JsonRetKind::ListListPrim(prim),
                    });
                }
                return Err(format!(
                    "return type not in dispatcher alphabet: list<list<{}>> (inner-inner not yet supported)",
                    type_label_dbg(inner2)
                ));
            }
            // W3.4 (#550) + W2 Phase 2 mop-up (#555): tuple-element
            // list returns. Two shapes wired today:
            //   - `list<tuple<geometry, f64>>` (postgis
            //     `st_dumpaspolygons`, `st_pixelaspolygons`) —
            //     hand-built JSON because Geometry is a resource.
            //   - `list<tuple<X1, X2, ...>>` over primitives (e.g.
            //     mobilitydb `datespanset_make -> list<tuple<s32,
            //     s32>>`) — direct `serde_json`.
            WitType::Tuple(elems) => {
                // (geometry, f64) special case.
                if elems.len() == 2
                    && matches!(elems[0], WitType::Geometry { .. })
                    && matches!(elems[1], WitType::F64)
                {
                    return Ok(RetShape::JsonText {
                        kind: JsonRetKind::ListTupleGeomF64,
                    });
                }
                // All-primitive tuples.
                let prims: Option<Vec<ListPrimElem>> =
                    elems.iter().map(list_prim_elem).collect();
                if let Some(prims) = prims {
                    if !prims.is_empty() {
                        return Ok(RetShape::JsonText {
                            kind: JsonRetKind::ListTuplePrim(prims),
                        });
                    }
                }
                let parts: Vec<String> = elems.iter().map(type_label_dbg).collect();
                return Err(format!(
                    "return type not in dispatcher alphabet: list<tuple<{}>> (tuple shape not yet wired)",
                    parts.join(", ")
                ));
            }
            other => {
                return Err(format!(
                    "return type not in dispatcher alphabet: list<{}> (inner not yet supported)",
                    type_label_dbg(other)
                ));
            }
        },
        // Phase F (#522): nested result inside another compound
        // — top-level result is handled via `WitRet.fallible`.
        WitType::Result(ok, _err) => {
            return Err(format!(
                "return type not in dispatcher alphabet: nested result<{}> (only top-level result is wired)",
                type_label_dbg(ok),
            ));
        }
        // Round 3: `bbox` record (4 f64s) is rendered as a WKB
        // POLYGON envelope so the interface DB's `binary` return
        // type is honoured. Implemented by composing the bridge's
        // existing `pg_ctor::st_make_envelope(xmin, ymin, xmax,
        // ymax)` constructor. Covers `st-make-box2d` and
        // `st-box-from-geohash`.
        WitType::Bbox => RetShape::BboxBlob,
        // Round 3: the specific tuple shape that
        // `st-is-valid-detail` returns — `tuple<bool,
        // option<string>, option<geometry>>` — is rendered as a
        // PostgreSQL composite-type text representation
        // `(valid, "reason", "POINT(x y)")` so the interface DB's
        // `text` return type is honoured.
        WitType::Tuple(elems)
            if elems.len() == 3
                && matches!(elems[0], WitType::Bool)
                && matches!(&elems[1], WitType::Option(t) if matches!(**t, WitType::String))
                && matches!(&elems[2], WitType::Option(t) if matches!(**t, WitType::Geometry { .. })) =>
        {
            RetShape::IsValidDetailText
        }
        // W3.5 (#551): `tuple<X1, X2, ...>` over primitive Xi —
        // serialise as JSON-array TEXT via serde. Covers postgis
        // `st-world-to-raster-coord -> tuple<s32, s32>` (a (col,
        // row) pair). The PostgreSQL surface for these functions
        // returns `text`, so JSON-array is a faithful rendering
        // SQL callers can unpack via `json_extract(...)`.
        WitType::Tuple(elems) => {
            let prims: Option<Vec<ListPrimElem>> =
                elems.iter().map(list_prim_elem).collect();
            if let Some(prims) = prims {
                if !prims.is_empty() {
                    return Ok(RetShape::JsonText {
                        kind: JsonRetKind::TuplePrim(prims),
                    });
                }
            }
            let parts: Vec<String> = elems.iter().map(type_label_dbg).collect();
            return Err(format!(
                "return type not in dispatcher alphabet: tuple<{}> (specific shape not yet wired)",
                parts.join(", ")
            ));
        }
        WitType::ListGeomBorrow => {
            return Err(format!(
                "return type not in dispatcher alphabet: list<borrow<geometry>> (impossible as return)"
            ));
        }
        WitType::ListRasterBorrow => {
            return Err(format!(
                "return type not in dispatcher alphabet: list<borrow<raster>> (impossible as return)"
            ));
        }
        // ListGeomOwned and ListOptionU32 handled above
        // (FirstGeomBlob / FirstOptionU32Int).
        WitType::Unsupported(s) => {
            // W3.3 (#543): WIT enums surface here for the same
            // reason params do — parse_type has no Enum variant.
            // Check enums before records (no overlap today; future-proof).
            if let Some(en) = enums.iter().find(|e| &e.decl.kebab_name == s) {
                let wit_module =
                    wit_parse::interface_to_rust_alias(&en.decl.interface).ok_or_else(|| {
                        format!(
                            "enum '{}' lives in interface '{}' with no Rust-binding alias",
                            s, en.decl.interface,
                        )
                    })?;
                return Ok(RetShape::Enum {
                    wit_module,
                    wit_package: en.package.clone(),
                    kebab_name: en.decl.kebab_name.clone(),
                    cases: en.decl.cases.clone(),
                });
            }
            // Phase E: record-typed return — wrap as wit-value.
            if let Some(rec) = records.iter().find(|r| &r.kebab_name == s) {
                let type_id_hex: String = rec
                    .type_id
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect();
                return Ok(RetShape::WitValueRecord {
                    kebab_name: rec.kebab_name.clone(),
                    wit_interface: rec.interface.clone(),
                    wit_package: rec.package.clone(),
                    wit_package_version: rec.package_version.clone(),
                    symbolic_name: rec.symbolic_name.clone(),
                    type_id_hex,
                });
            }
            return Err(format!("return type not in dispatcher alphabet: {s}"));
        }
    })
}

/// W2 Phase 2 mop-up (#555): canonical helper-name suffix for a
/// `ListTuple` signature. E.g. `[S32, S32]` → `"i32_i32"`. The
/// suffix matches `ListPrimElem::helper_suffix()` so the emitted
/// `parse_json_list_tuple_<sig>` helper sits in the same naming
/// family as the existing `parse_json_list_<suffix>` helpers.
pub fn list_tuple_sig_suffix(elements: &[ListPrimElem]) -> String {
    elements
        .iter()
        .map(|e| e.helper_suffix())
        .collect::<Vec<_>>()
        .join("_")
}

/// W2 (#542): classify the inner element of a `list<X>` param.
/// Returns Some for primitive elements (Phase 1 substrate);
/// None for record / geometry / span / topology elements
/// (deferred to wit-value list codec emission).
pub fn list_prim_elem(t: &WitType) -> Option<ListPrimElem> {
    match t {
        WitType::F64 => Some(ListPrimElem::F64),
        WitType::F32 => Some(ListPrimElem::F32),
        WitType::S32 => Some(ListPrimElem::S32),
        WitType::S64 => Some(ListPrimElem::S64),
        WitType::U32 => Some(ListPrimElem::U32),
        WitType::U64 => Some(ListPrimElem::U64),
        WitType::U8 => Some(ListPrimElem::U8),
        WitType::Bool => Some(ListPrimElem::Bool),
        WitType::String => Some(ListPrimElem::String),
        _ => None,
    }
}

/// Tiny re-impl of `wit_parse::type_label` for the error string
/// (the public version isn't exported as `pub`).
pub fn type_label_dbg(t: &WitType) -> String {
    match t {
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
        WitType::Option(inner) => format!("option<{}>", type_label_dbg(inner)),
        WitType::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(type_label_dbg).collect();
            format!("tuple<{}>", parts.join(", "))
        }
        WitType::Bbox => "bbox".into(),
        WitType::List(inner) => format!("list<{}>", type_label_dbg(inner)),
        WitType::Result(ok, _err) => format!("result<{}>", type_label_dbg(ok)),
        WitType::Unsupported(s) => s.clone(),
    }
}
pub fn clone_shape(s: &DispatchShape) -> DispatchShape {
    DispatchShape {
        wit_module: s.wit_module.clone(),
        wit_package: s.wit_package.clone(),
        wit_func: s.wit_func.clone(),
        params: s.params.clone(),
        ret: s.ret.clone(),
        method_call: s.method_call.clone(),
    }
}


/// #564: rewrite a classified return shape into `RetShape::TuplePick`
/// for the given tuple element index. Accepts the two shapes that
/// `classify_return` produces today for primitive tuples:
///
/// - bare `tuple<X1, X2, ...>` → `JsonText { TuplePrim(elems) }`
///   (W3.5 #551). This is the postgis surface for
///   `st-world-to-raster-coord -> tuple<s32, s32>`.
/// - `option<tuple<X1, X2, ...>>` → `JsonText { OptionTuplePrim(...) }`
///   isn't on the postgis surface today; bail clearly so a future
///   shim that adds an `option<tuple<...>>` accessor gets a named
///   diagnostic rather than silent miswiring.
///
/// Any other classified return is a category error — the SQL-name
/// override pointed at a function whose return isn't tuple-shaped.
/// Surface the mismatch as an unwired-symbol reason.
pub fn rewrite_ret_for_tuple_pick(ret: &RetShape, idx: usize) -> Result<RetShape, String> {
    match ret {
        RetShape::JsonText {
            kind: JsonRetKind::TuplePrim(elems),
        } => {
            let elem = elems.get(idx).copied().ok_or_else(|| {
                format!(
                    "tuple-pick index {idx} out of range for tuple of {} elements",
                    elems.len()
                )
            })?;
            Ok(RetShape::TuplePick { index: idx, elem })
        }
        RetShape::JsonText {
            kind: JsonRetKind::OptionTuplePrim(_),
        } => Err(format!(
            "tuple-pick override does not yet support option<tuple<...>> \
             returns (index {idx} requested); add an Option-aware variant \
             when the surface needs it"
        )),
        other => Err(format!(
            "tuple-pick override expects a tuple<...> return; underlying \
             function classifies as {:?}",
            other
        )),
    }
}


/// Public re-export so `emit_lib.rs` can decide which call form
/// to use. The boolean is the WIT-side `result<...>` flag.
pub fn build_full(
    plan: &BridgePlan,
    wit_deps_dir: &Path,
    records: &[RecordType],
) -> Result<(Vec<(DispatchEntry, bool)>, Vec<UnwiredScalar>)> {
    let wit_fns = wit_parse::parse_dir(wit_deps_dir)?;
    let aliases = collect_package_aliases(wit_deps_dir);
    let enums = collect_package_enums(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);
    let wit_index = index_wit_fns(&wit_fns);
    let wit_nohyphen = index_wit_fns_nohyphen(&wit_fns);
    // #547 (W3.1): resource-method index lookups when
    // free-function name matching misses. Keyed by
    // `<resource_snake>_<method_snake>` (e.g. `topology_node_count`).
    let method_index = index_resource_methods(&wit_fns);
    // #556 (W3.1 mop-up): resource → declaring-interface index for
    // the same-interface name-matching fallback (catches
    // `st_topologyfrombytes` → `postgis-topology-types::from-bytes`
    // via the `topology_from_bytes` alias).
    let resource_iface_index = index_resource_interfaces(&wit_fns);

    let mut entries: Vec<(DispatchEntry, bool)> = Vec::new();
    let mut unwired: Vec<UnwiredScalar> = Vec::new();

    for ext in &plan.extensions {
        for sc in &ext.scalars {
            let candidates = sql_name_candidates(sc);
            // 1) #564 tuple-pick override route — hand-curated SQL
            //    accessors (`st_worldtorastercoordcol/row`) that
            //    route to a tuple-returning WIT function and surface
            //    one element. Consulted FIRST so an explicit entry
            //    always wins over any incidental name collision.
            // 2) operator-override route (hand-curated names).
            // 3) standard snake/kebab resolution + Round-490
            //    prefix-stripping / no-hyphen lookup.
            // 4) #547 (W3.1) resource-method lookup against the
            //    `<resource>_<method>` index.
            // 5) #556 (W3.1 mop-up) same-interface name-matching:
            //    `<resource>_<func>` → free function `<func>` in
            //    the interface declaring `<resource>`.
            let tuple_pick = tuple_pick_override_for(&sc.canonical_name, &wit_fns);
            let matched: Option<&WitFunction> = if let Some((f, _)) = tuple_pick {
                Some(f)
            } else if let Some(f) = override_for(&sc.canonical_name, &wit_fns) {
                Some(f)
            } else if let Some(f) = find_wit_fn(&candidates, &wit_index, &wit_nohyphen) {
                Some(f)
            } else if let Some(f) = find_resource_method(&candidates, &method_index) {
                Some(f)
            } else {
                find_same_interface_free_fn(
                    &candidates,
                    &wit_index,
                    &resource_iface_index,
                )
            };
            let Some(f) = matched else {
                unwired.push(UnwiredScalar {
                    sql_name: sc.canonical_name.clone(),
                    reason: format!(
                        "no WIT function matches any of {:?}",
                        candidates
                    ),
                });
                continue;
            };
            match classify_shape(f, records, &enums) {
                Ok(mut shape) => {
                    // #564 tuple-pick: rewrite the classified return
                    // shape from the underlying tuple-of-primitives
                    // JSON-text variant to the per-element
                    // `RetShape::TuplePick`. The params stay as
                    // classify_shape produced them — the underlying
                    // function's signature is reused verbatim.
                    if let Some((_, idx)) = tuple_pick {
                        match rewrite_ret_for_tuple_pick(&shape.ret, idx) {
                            Ok(rewritten) => shape.ret = rewritten,
                            Err(reason) => {
                                unwired.push(UnwiredScalar {
                                    sql_name: sc.canonical_name.clone(),
                                    reason,
                                });
                                continue;
                            }
                        }
                    }
                    let fallible = f.ret.fallible;
                    entries.push((
                        DispatchEntry {
                            sql_name: sc.canonical_name.clone(),
                            shape: clone_shape(&shape),
                        },
                        fallible,
                    ));
                    for alias in &sc.aliases {
                        entries.push((
                            DispatchEntry {
                                sql_name: alias.clone(),
                                shape: clone_shape(&shape),
                            },
                            fallible,
                        ));
                    }
                }
                Err(reason) => unwired.push(UnwiredScalar {
                    sql_name: sc.canonical_name.clone(),
                    reason,
                }),
            }
        }
    }
    Ok((entries, unwired))
}
