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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::name_match::{
    aggregate_name_candidates, candidates_sorted, collect_package_aliases,
    collect_package_enums, find_dim_variant_match, find_resource_concat_match,
    find_resource_family_free_fn, find_resource_method, find_same_interface_free_fn,
    find_wit_fn,
    index_resource_interfaces, index_resource_methods, index_resource_methods_concat,
    index_wit_fns, index_wit_fns_nohyphen,
    resolve_function_aliases, sql_name_candidates, table_fn_name_candidates,
    AGGREGATE_NAME_SUFFIXES, EnumWithPackage,
};
use crate::override_tables::{aggregate_override_for, override_for, tuple_pick_override_for};
use crate::record_registry::RecordType;
use crate::wit_parse::{
    self, WitEnumDecl, WitFunction, WitParam, WitRet, WitType, WitTypeAlias,
};
use shim_bridge_codegen_core::{BridgePlan, ScalarFn, WindowFn};

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
        /// #710: `arg_witvalue_*` / `parse_json_list_record_*` /
        /// `ret_to_witvalue_*` suffix. Same as `kebab_name.replace('-','_')`
        /// unless the same kebab collides across interfaces in one
        /// package (mobilitydb's `stbox3d`), in which case the
        /// interface's snake form is prepended so the two variants
        /// get distinct wrapper functions.
        helper_snake: String,
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
        /// #710: helper-function suffix — see `WitValueRecord::helper_snake`.
        helper_snake: String,
    },
    /// #674: `list<list<u8>>` param — batched WKB blobs surfaced
    /// by postgis's `st_*_batch` family. SQL passes the value as
    /// JSON-text matching `Vec<Vec<u8>>` (nested arrays of byte
    /// integers, e.g. `'[[1,2,3], [4,5,6]]'`); the dispatch arm
    /// calls a codegen-emitted `parse_json_list_list_u8` helper
    /// and passes `&arg{idx}` (deref to `&[Vec<u8>]`) to the WIT
    /// function. JSON-of-int-arrays matches the symmetric
    /// `RetShape::JsonText { ListListPrim }` output convention.
    ListListU8,
    /// #695: `list<list<X>>` param for primitive non-u8 elements.
    /// `list<list<u8>>` keeps the dedicated `ListListU8` variant
    /// because the WIT parser surfaces `list<u8>` as
    /// `WitType::ListU8` (not `WitType::List(Box<U8>)`); every
    /// other primitive inner-inner type lands here. SQL passes a
    /// JSON-text matching `Vec<Vec<T>>` (e.g.
    /// `'[[1.0, 2.0], [3.0, 4.0]]'` for `list<list<f64>>`); the
    /// dispatch arm calls a codegen-emitted
    /// `parse_json_list_list_<elem>` helper and passes `&arg{idx}`
    /// (deref to `&[Vec<T>]`) to the WIT function. Today's surface:
    ///   - postgis `st-set-values` (`list<list<f64>>` values).
    ///   - flatgeobuf `make-polygon-with-holes` /
    ///     `make-multilinestring` (`list<list<f64>>` coords).
    /// Symmetric with `RetShape::JsonText { ListListPrim }`.
    ListListPrim(ListPrimElem),
    /// #781: `list<list<R>>` param where `R` is a record type
    /// declared in the shim's WIT. Extends the `ListRecord`
    /// pattern to the nested-list shape used by mobilitydb's
    /// spanset-extent-ops interface — the four
    /// `<int|float|date|tstz>-spanset-extent` scalars take
    /// `list<list<{int,float,date}-span>>` where the outer list
    /// is the batch of spansets and each inner list is one
    /// spanset's spans.
    ///
    /// SQL surface: JSON-array of arrays of record-shaped
    /// objects, e.g.
    /// `'[[{"lower":1,"upper":10,"lower-inc":true,"upper-inc":false}], ...]'`.
    ///
    /// Dispatch arm parses the TEXT via
    /// `serde_json::from_str::<Vec<Vec<UPSTREAM>>>` (wit-bindgen's
    /// `additional_derives: [Deserialize]` makes UPSTREAM records
    /// directly deserialisable; no LOCAL serde-ops codec is
    /// needed because dispatch is by func_id / name, not by
    /// type_id). The resulting `Vec<Vec<UPSTREAM>>` is passed to
    /// the WIT call as `&arg{idx}`.
    ///
    /// Mirrors the field layout of `ListRecord` so the
    /// emit_arm_body machinery re-uses the upstream-path lookup
    /// and the per-record helper suffix.
    ListListRecord {
        kebab_name: String,
        wit_interface: String,
        wit_package: String,
        wit_package_version: String,
        /// #710: helper-function suffix — see `ParamShape::WitValueRecord::helper_snake`.
        helper_snake: String,
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
    /// #724: `list<tuple<E1, E2, ...>>` param where at least one
    /// Ei is a same-shim record (rest may be primitives). Today's
    /// surface (mobilitydb): `tfloat-batch-to-parquet` /
    /// `tgeompoint-batch-to-parquet` — first arg is
    /// `list<tuple<string, {tfloat,tgeompoint}-sequence>>`.
    ///
    /// SQL surface: JSON-array of arrays, e.g.
    /// `'[["seq1", {"instants":[...], "interpolation":"linear", ...}]]'`.
    ///
    /// Dispatch arm parses via
    /// `serde_json::from_str::<Vec<(String, UPSTREAM_R)>>` — the
    /// per-signature helper `parse_json_list_tuple_<sig>` where
    /// `<sig>` mixes prim helper_suffix() and record helper_snake
    /// (e.g. `string_tfloat_sequence`). Records deserialize via
    /// wit-bindgen's `additional_derives: [serde::Deserialize]`;
    /// no LOCAL→UPSTREAM ciborium round-trip needed (dispatch is
    /// by func_id, not type_id — same reasoning as `ListRecord`).
    ListTupleMixed { elements: Vec<ListTupleElem> },
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

    /// #724: exposes the mixed tuple-element signature when this
    /// shape is `ListTupleMixed`, so emit_lib can de-duplicate
    /// per-signature `parse_json_list_tuple_<sig>` helpers that
    /// reference upstream record types (Vec<(String, UPSTREAM_R)>).
    pub fn list_tuple_mixed_sig(&self) -> Option<&[ListTupleElem]> {
        match self {
            ParamShape::ListTupleMixed { elements } => Some(elements.as_slice()),
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
    /// `Ok(SqlValue::Blob(<expr>.geometry().as_wkb()))` —
    /// topo-geometry result, routed through the resource's
    /// `geometry()` accessor and the geometry resource's existing
    /// WKB serializer. #707: the `postgis-topology-topogeom`
    /// resource has no direct `to-bytes` method; rendering the
    /// underlying MULTI* geometry is the canonical SQL-callable
    /// projection (downstream callers can chain `st_astext`,
    /// `st_npoints`, etc. against the WKB payload). Covers
    /// `create_topo_geom` / `topology_create_topo_geom`.
    TopoGeometryViaGeom,
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
    /// `bbox3d` record (6 f64s: min-x, min-y, min-z, max-x, max-y,
    /// max-z). Round (#608); reshaped for Gap G3 (#668). Rendered
    /// as an ISO-WKB `LINESTRING Z` blob whose two vertices are
    /// the bbox's min and max corners
    /// `(xmin, ymin, zmin) -> (xmax, ymax, zmax)`. The diagonal
    /// representation preserves all six coordinates and lets the
    /// downstream `st_astext` (and other scalar consumers) parse
    /// the aggregate's result as a standard WKB geometry. Today's
    /// only producer is `postgis-aggregates::st-extent-threed`
    /// (the `st_3dextent` SQL aggregate). Parallels `BboxBlob` for
    /// the 2D shape (which emits an ISO-WKB `POLYGON` envelope via
    /// `pg_ctor::st_make_envelope`); the 3D form is composed
    /// inline because no upstream WIT constructor builds a 3D
    /// envelope today.
    Bbox3dWkbLineZ,
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
        /// #710: helper-function suffix — see `ParamShape::WitValueRecord::helper_snake`.
        helper_snake: String,
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
        /// #710: helper-function suffix — see `ParamShape::WitValueRecord::helper_snake`.
        helper_snake: String,
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
        /// #710: helper-function suffix — see `ParamShape::WitValueRecord::helper_snake`.
        helper_snake: String,
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
    /// #716: `option<enum>` return. Some(variant) → Integer with the
    /// discriminant index; None → SQL NULL. Mirrors `Enum` on the
    /// Some side and `OptionInt` for the null projection. Today's
    /// surface: mobilitydb `tfloat-detect-trend -> option<trend-direction>`.
    OptionEnum {
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
    /// #677: `list<bool>` return — batched predicate result from
    /// postgis's `st_*_batch` predicate family. Encoded as
    /// SqlValue::Text holding a JSON array `[true,false,...]`.
    /// SQL callers consume via SQLite's JSON1 ops / DuckDB's
    /// `json_each`. Symmetric with the param-side
    /// `ParamShape::ListListU8` JSON convention; chose JSON over a
    /// first-element projection because the batch contract is
    /// "one input → one output" and surfacing only the first
    /// element silently drops data.
    ListBool,
    /// #677: `list<list<u8>>` return — batched geometry result
    /// from postgis's `st_*_batch` geometry family
    /// (`st_buffer_batch`, `st_centroid_batch`, etc.). Encoded
    /// as SqlValue::Text holding a JSON array of int-arrays
    /// (e.g. `[[1,2,3],[4,5,6]]`), symmetric with the param-side
    /// `ParamShape::ListListU8` convention.
    ListListU8,
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
    /// #690: unit OK type in a `result<_, E>` return (mutator
    /// surface — `remove-iso-node`, `change-edge-geom`, etc.).
    /// The call expression returns `()`, so the dispatch arm
    /// discards the value and yields `SqlValue::Null`.
    Unit,
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
    /// #630: `option<list<R>>` where `R` is a same-package record
    /// whose every field is a WIT primitive (`bool`, integer width,
    /// `f32`/`f64`, `string`) or `option<primitive>`. Some →
    /// `serde_json::to_string` on the inner `Vec<R>` (each element
    /// renders as a JSON object via the record's
    /// `serde::Serialize` derive, supplied by wit-bindgen's
    /// `additional_derives`); None → SQL NULL. The carried string
    /// is the record's kebab name — captured for diagnostics /
    /// codec-helper lookup, not for the emit template itself.
    /// Today's surface (mobilitydb):
    ///   - `date-spanset-from-text -> option<list<date-span>>`
    ///   - `float-spanset-from-text -> option<list<float-span>>`
    ///   - `int-spanset-from-text -> option<list<int-span>>`
    ///   - `tstz-spanset-from-text -> option<list<int-span>>`
    /// — plus the 4 mobilitydb-ducklink-bridge casts that route to
    /// these constructors and previously deferred because no scalar
    /// arm existed.
    OptionListPrimRecord(String),
    /// #799: `option<list<R>>` where `R` is a same-package record
    /// with nested compound fields (e.g. `list<record>`, which does
    /// not fit `OptionListPrimRecord`'s "record-of-primitives"
    /// constraint). The upstream Rust `Vec<R>` still serialises via
    /// serde — wit-bindgen's `additional_derives` supplies
    /// `Serialize` on every record (including transitively-nested
    /// ones) — so the emit template is byte-identical to
    /// `OptionListPrimRecord`; the split lets diagnostics /
    /// downstream tooling distinguish "flat rows" from "records
    /// with nested lists" without a structural walk.
    ///
    /// Today's surface (mobilitydb temporal-append-ops): the eight
    /// `t*-append-sequence(sset: list<t*-sequence>, seq: t*-sequence)
    ///   -> option<list<t*-sequence>>`
    /// signatures, where `t*-sequence` carries an `instants:
    /// list<t*-instant>` field (nested `list<record>`). SQL callers
    /// consume the JSON array of nested objects via SQLite's JSON1
    /// ops / DuckDB's `json_extract`.
    OptionListRecord(String),
    /// #716: `option<list<X>>` for primitive X — Some(vec) →
    /// `serde_json::to_string` on the inner `Vec<X>`; None → SQL NULL.
    /// Today's surface (mobilitydb): the four `*-set-from-text`
    /// constructors — `date-set-from-text -> option<list<s32>>`,
    /// `int-set-from-text -> option<list<s64>>`,
    /// `float-set-from-text -> option<list<f64>>`,
    /// `text-set-from-text -> option<list<string>>`,
    /// `tstz-set-from-text -> option<list<s64>>`.
    OptionListPrim(ListPrimElem),
    /// #716: `option<list<tuple<X1, X2, ...>>>` for primitive Xi —
    /// Some(vec) → `serde_json::to_string` on the inner
    /// `Vec<(X1, X2, ...)>` (serde renders Rust tuples as JSON
    /// arrays); None → SQL NULL. Today's surface (mobilitydb):
    /// `parse-geojson-linestring -> option<list<tuple<f64, f64>>>`.
    OptionListTuplePrim(Vec<ListPrimElem>),
    /// #716: `option<tuple<E1, E2, ...>>` where each Ei is a
    /// primitive OR `option<primitive>`. Extends `OptionTuplePrim`
    /// (which requires every element to be a bare primitive) so
    /// signatures like `option<tuple<f64, f64, option<u32>>>` from
    /// `parse-wkb-point` can wire. Serde renders `Option<X>` fields
    /// as JSON `null` on the None side, matching SQL callers'
    /// existing `json_extract` handling for missing values.
    OptionTuplePrimOrOptPrim(Vec<TupleElemKind>),
    /// #724: `list<tuple<E1, E2, ...>>` where at least one Ei is a
    /// same-shim record (rest may be primitives). Today's surface
    /// (mobilitydb): `tfloat-batch-from-parquet` /
    /// `tgeompoint-batch-from-parquet` — both return
    /// `result<list<tuple<string, {tfloat,tgeompoint}-sequence>>, arrow-error>`.
    /// serde_json::to_string renders the outer `Vec<(String, UPSTREAM_R)>`
    /// directly — records carry `serde::Serialize` via wit-bindgen's
    /// `additional_derives` so no per-element hand-marshaling is
    /// needed. Same emit template as `ListTuplePrim`.
    ListTupleMixed(Vec<ListTupleElem>),
}

/// #716: element kind for tuple returns that mix bare primitives
/// with `option<primitive>` fields. Emits as one of the standard
/// serde-rendered forms — `Some(x)` / `None` for the optional
/// arm, direct value for the bare arm — inside the parent
/// `Vec<(...)>` or `(...)` tuple. Used by
/// `JsonRetKind::OptionTuplePrimOrOptPrim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TupleElemKind {
    Prim(ListPrimElem),
    OptionPrim(ListPrimElem),
}

/// #724: element kind for `list<tuple<...>>` param/return shapes
/// that admit either a primitive or a same-shim record. Records
/// carry the same identity fields as `ParamShape::ListRecord` /
/// `ParamShape::WitValueRecord` — kebab + interface + package +
/// helper_snake — so the emit paths can reconstruct the upstream
/// Rust type (`bindings::<ns>::<name>::<iface>::<Pascal>`) and
/// pick the disambiguated helper suffix (#709/#710).
#[derive(Debug, Clone)]
pub enum ListTupleElem {
    Prim(ListPrimElem),
    Record(TupleRecordRef),
}

/// #724: identity fields for a same-shim record referenced from
/// inside a `list<tuple<...>>` shape. Mirrors the
/// `ParamShape::ListRecord` field set (kebab, interface, package,
/// package_version, helper_snake) so upstream-path resolution in
/// the emit crates uses the same inputs and helper-name
/// disambiguation stays in lock-step with the #709/#710 machinery.
#[derive(Debug, Clone)]
pub struct TupleRecordRef {
    pub kebab_name: String,
    pub wit_interface: String,
    pub wit_package: String,
    pub wit_package_version: String,
    pub helper_snake: String,
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
    // mosaic aggregates live here) plus `temporal-aggregate-ops`
    // (#607 Phase 0; mobilitydb temporal-type aggregates).
    // Fallback index: any other interface whose function's first
    // param is a borrowed-list of a supported resource type, OR an
    // owned-list of a record-typed kebab — covers
    // `postgis-accessors::st-collect` and friends that are declared
    // as scalars but semantically usable as aggregates.
    fn is_primary_agg_interface(iface: &str) -> bool {
        matches!(
            iface,
            "postgis-aggregates"
                | "postgis-raster-aggregates"
                | "temporal-aggregate-ops"
                // #799: mobilitydb's span-union-ops carries both the
                // 4 flat `list<X-span>` union aggs (already wired via
                // AccKind::Record) and the 4 nested
                // `list<list<X-span>> -> list<X-span>` spanset union
                // aggs (this issue: `<int|float|date|tstz>-spanset-
                // aggregate-union`). Both fit the aggregate model; the
                // classifier picks the `RecordSetToRecordSet` branch
                // for the nested shape.
                | "span-union-ops"
        )
    }
    let mut agg_index: HashMap<String, &WitFunction> = HashMap::new();
    for f in &wit_fns {
        if is_primary_agg_interface(&f.interface) {
            agg_index.insert(wit_parse::kebab_to_snake(&f.kebab_name), f);
        }
    }
    let mut agg_index_fallback: HashMap<String, &WitFunction> = HashMap::new();
    for f in &wit_fns {
        if is_primary_agg_interface(&f.interface) {
            continue;
        }
        let first_is_agg_list = f
            .params
            .first()
            .map(|p| match &p.ty {
                WitType::ListGeomBorrow | WitType::ListRasterBorrow => true,
                // #607 Phase 0: list<X> owned where X is unsupported
                // (i.e. a kebab — record candidate). The classifier
                // will validate the kebab against the record registry.
                //
                // #799: also accept list<list<X>> where X is a record
                // kebab — the `<int|float|date|tstz>-spanset-
                // aggregate-union` shape. `span-union-ops` is on the
                // primary allowlist, but the fallback index catches
                // any future interface that uses the same nested
                // shape without needing another allowlist patch.
                WitType::List(inner) => match inner.as_ref() {
                    WitType::Unsupported(_) => true,
                    WitType::List(inner2) => {
                        matches!(inner2.as_ref(), WitType::Unsupported(_))
                    }
                    _ => false,
                },
                _ => false,
            })
            .unwrap_or(false);
        if first_is_agg_list {
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
            // Round (#608): hand-curated overrides win before
            // candidate-list lookup so a SQL aggregate whose
            // canonical/alias forms share no stem with the upstream
            // WIT kebab still wires. Sibling to `override_for` on
            // the scalar path.
            if let Some(f) = aggregate_override_for(&ag.canonical_name, &wit_fns) {
                matched = Some(f);
            }
            if matched.is_none() {
                for cand in &candidates {
                    if let Some(f) = agg_index.get(cand) {
                        matched = Some(*f);
                        break;
                    }
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
                    // Phase 1A: emit one canonical AggregateEntry with
                    // its alias list folded into the `aliases` field.
                    // Previously we pushed one entry per canonical PLUS
                    // one per alias — the duplicate-entry shape forced
                    // every downstream emitter to dedupe by sql_name
                    // (and the datafission target additionally needed
                    // an alias-vs-canonical name index, #650 Path C).
                    // The new shape carries the alias list inline; emit
                    // sites expand aliases at the use site so SQLite /
                    // DuckDB per-alias dispatch arms and datafission
                    // metadata stay byte-identical.
                    entries.push(AggregateEntry {
                        sql_name: ag.canonical_name.clone(),
                        shape,
                        aliases: ag.aliases.clone(),
                    });
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

/// #667 (G2): augment a `BridgePlan` with synthetic `AggregateFn`
/// entries for SQL names that appear in the
/// `aggregate_function_overrides` table but are missing from the
/// interface DB's `aggregates` rows.
///
/// The interface DB is populated by extracting a composed shim
/// wasm via `aggregate-function-registry::list-functions` — a
/// chicken-and-egg loop with the regenerated datafission adapter.
/// A new SQL aggregate (e.g. `st_extent` after P4 landed in
/// postgis-wasm `b2534f1`) cannot enter the loop without a re-
/// extract, which the per-target regen here does not perform.
/// Treating the aggregate override table as a complementary seed
/// source closes the gap: any (sql_name, wit_interface, wit_kebab)
/// tuple whose WIT function exists is added to the primary
/// extension's aggregate list before the per-target emit pass
/// walks it. Downstream pieces — manifest metadata, `agg_id_for`
/// index, registry classification, alias dispatch — then see a
/// uniform `plan.extensions[].aggregates` shape and no longer
/// have to be override-aware.
///
/// Synthetic entries carry:
///   - `param_signatures = [[binary]]` (single streaming WKB
///     column; extras still come from the WIT shape downstream)
///   - `aliases = []` (synonyms get their own override rows)
///   - `supports_grouped = true`, `supports_partial = true`,
///     `is_order_sensitive = false`, `accepts_config = false`
///
/// Pre-existing override entries that ARE in the interface DB
/// (e.g. `st_3dextent`) are unaffected — the dedupe check skips
/// any SQL name already present in `plan.extensions[].aggregates`.
pub fn augment_plan_with_override_aggregates(
    plan: &mut shim_bridge_codegen_core::BridgePlan,
    wit_deps_dir: &Path,
) -> Result<()> {
    if plan.extensions.is_empty() {
        return Ok(());
    }

    // The supplied path is the wit/deps/-shaped ROOT (each shim
    // package lives in a subdir like `postgis-wasm/`,
    // `sfcgal-component/`, etc.). `wit_parse::parse_dir` parses
    // exactly one directory of .wit files, so walk one level down
    // and concatenate. Mirrors the per-target emit_lib's
    // `pick_primary_shim_dir + parse_dir` pair but consolidates
    // because the augment pass only needs to find aggregate-shaped
    // WIT functions across every shim package, not pick a single
    // primary subdir.
    let mut wit_fns = Vec::<wit_parse::WitFunction>::new();
    if let Ok(rd) = std::fs::read_dir(wit_deps_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let parsed = wit_parse::parse_dir(&p)?;
            wit_fns.extend(parsed);
        }
    }
    // Also try the supplied dir directly in case the caller
    // already drilled into a single shim package.
    let direct = wit_parse::parse_dir(wit_deps_dir).unwrap_or_default();
    wit_fns.extend(direct);
    let aliases = collect_package_aliases(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);

    // Collect every SQL aggregate name already wired across every
    // extension. Override entries do not declare which extension
    // owns them; attach synthesised entries to the primary (first)
    // extension since that matches the convention
    // `build_aggregate_registry` uses for its single-pass walk.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ext in &plan.extensions {
        for ag in &ext.aggregates {
            seen.insert(ag.canonical_name.clone());
            for alias in &ag.aliases {
                seen.insert(alias.clone());
            }
        }
    }

    let mut synthetic = Vec::<shim_bridge_codegen_core::AggregateFn>::new();
    for (sql_name, wit_iface, wit_kebab) in
        crate::override_tables::aggregate_function_overrides()
    {
        if seen.contains(*sql_name) {
            continue;
        }
        // Only synthesise when the WIT function actually exists.
        // Overrides may be queued ahead of an upstream pin bump;
        // skip silently in that case.
        if !wit_fns
            .iter()
            .any(|wf| wf.interface == *wit_iface && wf.kebab_name == *wit_kebab)
        {
            continue;
        }
        seen.insert(sql_name.to_string());
        synthetic.push(shim_bridge_codegen_core::AggregateFn {
            canonical_name: sql_name.to_string(),
            aliases: Vec::new(),
            param_signatures: vec![vec!["binary".to_string()]],
            supports_grouped: true,
            supports_partial: true,
            is_order_sensitive: false,
            accepts_config: false,
            config_arg_indices: Vec::new(),
        });
    }

    if !synthetic.is_empty() {
        eprintln!(
            "[codegen] augment-plan: synthesised {} aggregate(s) from override table",
            synthetic.len(),
        );
        for s in &synthetic {
            eprintln!("  + {}", s.canonical_name);
        }
        plan.extensions[0].aggregates.extend(synthetic);
        // Keep the aggregate list sorted by canonical_name so the
        // emitted dispatch arms stay deterministic across regens
        // (matches the alphabetical ordering `load_plan` produces
        // via `ORDER BY name`).
        plan.extensions[0]
            .aggregates
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    Ok(())
}

/// #680: complement to `augment_plan_with_override_aggregates` for the
/// SCALAR surface. Closes the codegen substrate gap that prevented
/// upstream postgis-wasm WIT additions from surfacing as SQL functions.
///
/// ## The gap
///
/// The codegen reads its scalar list from the extracted interface DB,
/// which is populated by calling the postgis-datafission-bridge's
/// `list_functions()` impl. That impl is HARDCODED at bridge-regen
/// time — once a new SQL function is added upstream in postgis-wasm
/// (e.g. the Tier 1 SFCGAL, topology, face-split ops added in #681
/// and #682), the existing bridge crate's frozen `vec![]` doesn't
/// know about it, so the next interface-DB extraction silently drops
/// it on the floor. Codegen then iterates `ext.scalars` without ever
/// walking the upstream WIT for late additions, and the regenerated
/// bridge still doesn't include the new function.
///
/// ## The fix
///
/// Mirror the override-aggregate augment pattern but seed from the
/// upstream WIT directly. For every WIT function the parser sees,
/// compute a snake-case SQL name (`st-alpha-shape` → `st_alpha_shape`)
/// and synthesise a `ScalarFn` row when that name isn't already on
/// the plan (regardless of which surface — scalars, aggregates, table
/// functions, window functions — owns it today). Resource methods are
/// skipped: their SQL surface is `<resource>_<method>` and the
/// dispatch matcher resolves them through `index_resource_methods`
/// when the interface DB actually carries the row — synthesising
/// them here would produce free-function-shaped SQL names that don't
/// match the resource-method dispatch convention.
///
/// ## Synthesised shape
///
/// Each synthetic row carries:
///   - `param_signatures = [[binary; arity]]` — only the arity is
///     consumed downstream (by `scalar_num_args` in the SQLite emit's
///     metadata pass); the `"binary"` placeholders never reach the
///     actual call-marshalling pipeline because the dispatch matcher
///     re-classifies the param shapes against the WIT signature.
///   - `return_type = "binary"` — same reason; not threaded into emit.
///   - `is_deterministic = true`, `propagates_null = true` — the
///     common case for PostGIS scalars; mismatches would surface as
///     incorrect SQL-function flags on the SQLite registration, not
///     wrong call behaviour.
///   - `aliases = []` — synonyms get their own interface-DB rows on
///     the next regen once the bridge advertises them.
///
/// ## Why this layers on top of the override path
///
/// `augment_plan_with_override_aggregates` requires a hand-curated
/// (sql_name, wit_interface, wit_kebab) tuple per op. That works for
/// the aggregate surface (small, slow-growth) but the scalar surface
/// regularly absorbs 30+ ops per upstream release. Auto-synthesis
/// removes the manual maintenance step entirely: any new WIT function
/// surfaces immediately on the next codegen run.
pub fn augment_plan_with_upstream_wit_scalars(
    plan: &mut shim_bridge_codegen_core::BridgePlan,
    wit_deps_dir: &Path,
) -> Result<()> {
    if plan.extensions.is_empty() {
        return Ok(());
    }

    // Same two-pass walk as `augment_plan_with_override_aggregates`:
    // the supplied path may be a `wit/deps/`-shaped root containing
    // per-package subdirs OR an already-drilled-in single package
    // dir; walk both shapes so the call site doesn't have to choose.
    let mut wit_fns = Vec::<wit_parse::WitFunction>::new();
    if let Ok(rd) = std::fs::read_dir(wit_deps_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let parsed = wit_parse::parse_dir(&p)?;
            wit_fns.extend(parsed);
        }
    }
    let direct = wit_parse::parse_dir(wit_deps_dir).unwrap_or_default();
    wit_fns.extend(direct);
    let aliases = collect_package_aliases(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);

    // #706: drop helper-package WIT functions before synthesising
    // SQL scalars. The synthesiser used to walk every WIT package
    // under `wit_deps_dir` (postgis-wasm + sfcgal-component +
    // gdal-core + geos-geometry + proj-wasm + every other helper
    // imported via wac plug), producing ~600 spurious bare-name
    // entries (`alpha_shape`, `area`, `buffer`, `centroid`, ...)
    // that have no matching free function in the primary shim's
    // WIT. The dispatcher then walks `pick_primary_shim_dir(...)`
    // ONLY — so these synthetics all unwire with
    // "no WIT function matches", flooding the regen log without
    // adding any SQL surface (the real `st_alpha_shape`,
    // `st_area`, `st_buffer`, ... entries already exist in the
    // interface DB and wire fine through `postgis-wasm/sfcgal.wit`
    // etc.).  Keep only functions whose owning WIT package belongs
    // to the primary extension's namespace (e.g. for `postgis`,
    // only `postgis:wasm`; for `mobilitydb`, only `mobilitydb:*`).
    let primary = plan.extensions[0].name.clone();
    let wit_fns: Vec<wit_parse::WitFunction> = wit_fns
        .into_iter()
        .filter(|f| {
            f.package
                .split(':')
                .next()
                .map(|ns| ns == primary)
                .unwrap_or(false)
        })
        .collect();

    // Collect every SQL name already wired across every extension
    // and every function category. A new WIT function whose
    // kebab→snake name collides with an existing aggregate /
    // table fn / window fn is left alone — the existing entry's
    // dispatch path is correct, and synthesising a duplicate scalar
    // row would only confuse the SQLite emit's metadata enumeration.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            seen.insert(sc.canonical_name.clone());
            for a in &sc.aliases {
                seen.insert(a.clone());
            }
        }
        for ag in &ext.aggregates {
            seen.insert(ag.canonical_name.clone());
            for a in &ag.aliases {
                seen.insert(a.clone());
            }
        }
        for tf in &ext.table_functions {
            seen.insert(tf.canonical_name.clone());
            for a in &tf.aliases {
                seen.insert(a.clone());
            }
        }
        for w in &ext.window_functions {
            seen.insert(w.canonical_name.clone());
            for a in &w.aliases {
                seen.insert(a.clone());
            }
        }
    }

    let mut synthetic = Vec::<shim_bridge_codegen_core::ScalarFn>::new();
    let mut synthetic_aggregates = Vec::<shim_bridge_codegen_core::AggregateFn>::new();
    let mut synthetic_windows = Vec::<shim_bridge_codegen_core::WindowFn>::new();
    for f in &wit_fns {
        // Resource methods don't sit on the free-function-shaped SQL
        // surface — they're called as `<resource>_<method>` and the
        // dispatch path resolves them through `index_resource_methods`
        // only when the interface DB has the row. Synthesising
        // bare-method names here would publish them as standalone
        // SQL scalars, which isn't the upstream contract. Constructors
        // (kebab `create-<resource>`) ARE free-function-shaped (e.g.
        // `st_createtopology`) so they pass through.
        if f.resource.is_some() && !f.is_constructor {
            continue;
        }
        let bare = wit_parse::kebab_to_snake(&f.kebab_name);
        // Arity-correct placeholder signature. Only `scalar_num_args`
        // reads `param_signatures` on the synthesised path; the
        // dispatch matcher re-classifies the real shapes against the
        // WIT signature when it walks `ext.scalars`.
        let arity = f.params.len();
        let param_sig: Vec<String> =
            std::iter::repeat("binary".to_string()).take(arity).collect();
        let make_row = |name: String| shim_bridge_codegen_core::ScalarFn {
            canonical_name: name,
            aliases: Vec::new(),
            param_signatures: vec![param_sig.clone()],
            return_type: "binary".to_string(),
            is_deterministic: true,
            propagates_null: true,
        };
        // #768: shape-classifier for the WIT-scalar auto-synthesis.
        // Before this fix every unseen WIT function was published as a
        // `ScalarFn`, so upstream aggregate / window entries that were
        // absent from the interface DB's `aggregates` / `window_functions`
        // tables (any recent postgis-wasm addition ahead of an interface
        // re-extract) leaked into the SQLite manifest under
        // `ScalarFunctionSpec`. The postgis category audit (#767) surfaced
        // ~9 such fns:
        //   - `-aggregate` / `-agg` suffix or a primary aggregate
        //     interface (`postgis-aggregates`, `postgis-raster-aggregates`,
        //     `temporal-aggregate-ops`) → route to `plan.aggregates`
        //   - `-win` suffix or window-shape signature
        //     (list<borrow<geometry>> → list<option<u32>|u32|geometry>)
        //     → route to `plan.window_functions`
        //   - anything else → keep on `scalars` (existing behaviour)
        //
        // Detection is intentionally cheap and conservative — false
        // positives would down-route a real scalar into the aggregate
        // path where `classify_aggregate_shape` would then unwire it
        // with an informative reason (surfaced in the codegen log),
        // rather than silently mis-emitting a wrong spec.
        let category = classify_wit_fn_category(f);
        if !seen.contains(&bare) {
            seen.insert(bare.clone());
            match category {
                WitFnCategory::Scalar => synthetic.push(make_row(bare.clone())),
                WitFnCategory::Aggregate => {
                    synthetic_aggregates.push(shim_bridge_codegen_core::AggregateFn {
                        canonical_name: bare.clone(),
                        aliases: Vec::new(),
                        param_signatures: vec![param_sig.clone()],
                        supports_grouped: true,
                        supports_partial: true,
                        is_order_sensitive: false,
                        accepts_config: false,
                        config_arg_indices: Vec::new(),
                    });
                }
                WitFnCategory::Window => {
                    synthetic_windows.push(shim_bridge_codegen_core::WindowFn {
                        canonical_name: bare.clone(),
                        aliases: Vec::new(),
                        param_signatures: vec![param_sig.clone()],
                    });
                }
            }
        }
        // #690: when a free function in a `<ns>-<resource>-*` family
        // interface takes `borrow<resource>` as its first parameter,
        // the established SQL convention (per the OLD hand-written
        // bridge `list_functions()`) prefixes it with `<resource>_`.
        // Example: `add-node` in `postgis-topology-edit` takes
        // `borrow<topology>` and surfaces as `topology_add_node`,
        // matching siblings like `topology_add_iso_node`,
        // `topology_mod_edge_heal`, etc.
        //
        // Skip when the kebab already starts with `<resource>-`
        // (e.g. `topology-summary` kebabs straight to
        // `topology_summary`) or with `st-` (PostGIS-style names
        // already follow the `st_<verb>` convention and don't take
        // a resource prefix). The dispatcher's
        // `find_resource_family_free_fn` resolves the prefixed name
        // against the unprefixed WIT kebab in any `<ns>-<resource>-*`
        // interface.
        if let Some(resource_kebab) = resource_family_prefix_for(f) {
            let bare_kebab = f.kebab_name.as_str();
            let resource_kebab_dash = format!("{}-", resource_kebab);
            let already_prefixed = bare_kebab.starts_with(&resource_kebab_dash);
            let st_prefixed = bare_kebab.starts_with("st-");
            if !already_prefixed && !st_prefixed {
                let prefixed = format!(
                    "{}_{}",
                    resource_kebab.replace('-', "_"),
                    bare,
                );
                if !seen.contains(&prefixed) {
                    seen.insert(prefixed.clone());
                    synthetic.push(make_row(prefixed));
                }
            }
        }
    }

    if !synthetic.is_empty() {
        eprintln!(
            "[codegen] augment-plan: synthesised {} scalar(s) from upstream WIT",
            synthetic.len(),
        );
        for s in &synthetic {
            eprintln!("  + {}", s.canonical_name);
        }
        plan.extensions[0].scalars.extend(synthetic);
        // Keep the scalar list deterministically ordered — the SQLite
        // emit's metadata pass assigns sequential ids walking
        // `ext.scalars` in order, so changing the order would churn
        // the generated function ids across regens. Match the
        // alphabetical ordering `load_plan` produces via `ORDER BY name`.
        plan.extensions[0]
            .scalars
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    // #768: same deterministic-order convention for the aggregate /
    // window slots the shape classifier now routes to.
    if !synthetic_aggregates.is_empty() {
        eprintln!(
            "[codegen] augment-plan: synthesised {} aggregate(s) from upstream WIT (#768)",
            synthetic_aggregates.len(),
        );
        for a in &synthetic_aggregates {
            eprintln!("  + {}", a.canonical_name);
        }
        plan.extensions[0].aggregates.extend(synthetic_aggregates);
        plan.extensions[0]
            .aggregates
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    if !synthetic_windows.is_empty() {
        eprintln!(
            "[codegen] augment-plan: synthesised {} window function(s) from upstream WIT (#768)",
            synthetic_windows.len(),
        );
        for w in &synthetic_windows {
            eprintln!("  + {}", w.canonical_name);
        }
        plan.extensions[0].window_functions.extend(synthetic_windows);
        plan.extensions[0]
            .window_functions
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    Ok(())
}

/// #790: reclassify EXISTING interface-DB entries against
/// `classify_wit_fn_category`. Complements
/// `augment_plan_with_upstream_wit_scalars`, which only routes newly-
/// synthesized WIT fns through the classifier and leaves rows the
/// datafission adapter already registered in whatever bucket the
/// interface-DB extractor stamped them under.
///
/// Motivation: when the datafission adapter's `list_functions()`
/// registers a fn as a scalar because the shim historically classed
/// it that way (e.g. `int_span_aggregate_union` — landed as scalar
/// pre-#782 because the tail-only aggregate detector missed the
/// `-aggregate-` infix), the shim-interface extractor writes it into
/// `scalars` in the interface DB. Re-extraction alone can't fix that
/// because the extractor mirrors whatever the adapter advertises; and
/// codegen alone can't fix that because `load_plan` reads the
/// interface DB as-is. #782 landed the infix classifier but only new
/// (unwired-in-prior-regen) variants got moved on last regen — the
/// ~49 aggregates the datafission adapter already knew about were
/// still stuck as scalars, producing a +4 wire delta instead of ~53.
///
/// The fix walks each extension's `scalars` / `aggregates` /
/// `window_functions` lists, looks up the WIT function by
/// `snake ← kebab` name, and moves any entry whose classifier verdict
/// disagrees with its current bucket. Aliases and param signatures
/// carry over; the aggregate / window-specific fields (support flags,
/// order-sensitive, config-arg indices) use the same defaults the
/// synthesis path uses (`supports_grouped = true`, etc.), matching
/// what a fresh interface-DB row would look like if the adapter
/// registered the fn under the correct bucket.
///
/// This is complementary to `augment_plan_with_upstream_wit_scalars`:
/// reclassify first (fix wrong buckets), then augment (add new). Both
/// re-sort their target lists so the SQLite emit's metadata pass sees
/// a deterministic order across regens.
pub fn reclassify_plan_categories_against_wit(
    plan: &mut shim_bridge_codegen_core::BridgePlan,
    wit_deps_dir: &Path,
) -> Result<()> {
    if plan.extensions.is_empty() {
        return Ok(());
    }

    // Same two-pass walk as `augment_plan_with_upstream_wit_scalars`
    // (per-package subdir + single-package fallback), same primary-
    // namespace filter — the classifier only knows about fns that
    // are actually reachable on the primary shim's WIT surface.
    let mut wit_fns = Vec::<wit_parse::WitFunction>::new();
    if let Ok(rd) = std::fs::read_dir(wit_deps_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let parsed = wit_parse::parse_dir(&p)?;
            wit_fns.extend(parsed);
        }
    }
    let direct = wit_parse::parse_dir(wit_deps_dir).unwrap_or_default();
    wit_fns.extend(direct);
    let aliases = collect_package_aliases(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);

    let primary = plan.extensions[0].name.clone();
    let wit_fns: Vec<wit_parse::WitFunction> = wit_fns
        .into_iter()
        .filter(|f| {
            f.package
                .split(':')
                .next()
                .map(|ns| ns == primary)
                .unwrap_or(false)
        })
        .collect();

    // Index by kebab→snake name — that's how interface-DB rows are
    // keyed, and how `augment_plan_with_upstream_wit_scalars` matches
    // synthesis candidates against the `seen` set.
    let mut by_snake: HashMap<String, &wit_parse::WitFunction> = HashMap::new();
    for f in &wit_fns {
        // Free-function-shaped rows only — resource methods surface
        // under `<resource>_<method>` names and are dispatched via a
        // separate index; reclassifying them here would rename them.
        if f.resource.is_some() && !f.is_constructor {
            continue;
        }
        let snake = wit_parse::kebab_to_snake(&f.kebab_name);
        by_snake.entry(snake).or_insert(f);
    }

    // For each plan bucket, collect the entries that belong in a
    // different bucket, then move them in a second pass.
    #[derive(Default)]
    struct Moves {
        scalars_out: Vec<usize>,
        aggregates_out: Vec<usize>,
        windows_out: Vec<usize>,
        new_scalars: Vec<shim_bridge_codegen_core::ScalarFn>,
        new_aggregates: Vec<shim_bridge_codegen_core::AggregateFn>,
        new_windows: Vec<shim_bridge_codegen_core::WindowFn>,
    }

    let ext = &mut plan.extensions[0];
    let mut moves = Moves::default();

    // Helper: default AggregateFn / WindowFn shapes when converting
    // FROM a scalar / into a new bucket. Mirrors the defaults used
    // by `augment_plan_with_upstream_wit_scalars` for synthesized
    // rows (support flags true, order-insensitive, no config args).
    let to_aggregate = |name: String, aliases: Vec<String>, sigs: Vec<Vec<shim_bridge_codegen_core::TypeName>>| shim_bridge_codegen_core::AggregateFn {
        canonical_name: name,
        aliases,
        param_signatures: sigs,
        supports_grouped: true,
        supports_partial: true,
        is_order_sensitive: false,
        accepts_config: false,
        config_arg_indices: Vec::new(),
    };
    let to_window = |name: String, aliases: Vec<String>, sigs: Vec<Vec<shim_bridge_codegen_core::TypeName>>| shim_bridge_codegen_core::WindowFn {
        canonical_name: name,
        aliases,
        param_signatures: sigs,
    };
    let to_scalar =
        |name: String, aliases: Vec<String>, sigs: Vec<Vec<shim_bridge_codegen_core::TypeName>>| shim_bridge_codegen_core::ScalarFn {
            canonical_name: name,
            aliases,
            param_signatures: sigs,
            // Placeholder return + flags mirror the synthesised
            // scalar defaults (`augment_plan_with_upstream_wit_scalars`).
            // The dispatch matcher re-classifies against the WIT sig
            // downstream and never reads these on the reclassified path.
            return_type: "binary".to_string(),
            is_deterministic: true,
            propagates_null: true,
        };

    // Walk scalars — anything WIT-classified as Aggregate or Window
    // moves out.
    for (i, sc) in ext.scalars.iter().enumerate() {
        if let Some(f) = by_snake.get(sc.canonical_name.as_str()) {
            match classify_wit_fn_category(f) {
                WitFnCategory::Scalar => {}
                WitFnCategory::Aggregate => {
                    moves.scalars_out.push(i);
                    moves.new_aggregates.push(to_aggregate(
                        sc.canonical_name.clone(),
                        sc.aliases.clone(),
                        sc.param_signatures.clone(),
                    ));
                }
                WitFnCategory::Window => {
                    moves.scalars_out.push(i);
                    moves.new_windows.push(to_window(
                        sc.canonical_name.clone(),
                        sc.aliases.clone(),
                        sc.param_signatures.clone(),
                    ));
                }
            }
        }
    }
    // Walk aggregates — anything WIT-classified as Scalar or Window
    // moves out. (Scalar case is exceedingly rare — an -aggregate
    // suffix that isn't actually an aggregate — but the classifier
    // is the source of truth.)
    for (i, ag) in ext.aggregates.iter().enumerate() {
        if let Some(f) = by_snake.get(ag.canonical_name.as_str()) {
            match classify_wit_fn_category(f) {
                WitFnCategory::Aggregate => {}
                WitFnCategory::Scalar => {
                    moves.aggregates_out.push(i);
                    moves.new_scalars.push(to_scalar(
                        ag.canonical_name.clone(),
                        ag.aliases.clone(),
                        ag.param_signatures.clone(),
                    ));
                }
                WitFnCategory::Window => {
                    moves.aggregates_out.push(i);
                    moves.new_windows.push(to_window(
                        ag.canonical_name.clone(),
                        ag.aliases.clone(),
                        ag.param_signatures.clone(),
                    ));
                }
            }
        }
    }
    // Walk window fns — anything WIT-classified as Scalar or
    // Aggregate moves out.
    for (i, w) in ext.window_functions.iter().enumerate() {
        if let Some(f) = by_snake.get(w.canonical_name.as_str()) {
            match classify_wit_fn_category(f) {
                WitFnCategory::Window => {}
                WitFnCategory::Scalar => {
                    moves.windows_out.push(i);
                    moves.new_scalars.push(to_scalar(
                        w.canonical_name.clone(),
                        w.aliases.clone(),
                        w.param_signatures.clone(),
                    ));
                }
                WitFnCategory::Aggregate => {
                    moves.windows_out.push(i);
                    moves.new_aggregates.push(to_aggregate(
                        w.canonical_name.clone(),
                        w.aliases.clone(),
                        w.param_signatures.clone(),
                    ));
                }
            }
        }
    }

    let moved_count = moves.new_scalars.len()
        + moves.new_aggregates.len()
        + moves.new_windows.len();
    if moved_count == 0 {
        return Ok(());
    }

    // Remove-in-reverse so earlier indices stay valid.
    for i in moves.scalars_out.into_iter().rev() {
        ext.scalars.remove(i);
    }
    for i in moves.aggregates_out.into_iter().rev() {
        ext.aggregates.remove(i);
    }
    for i in moves.windows_out.into_iter().rev() {
        ext.window_functions.remove(i);
    }

    if !moves.new_aggregates.is_empty() {
        eprintln!(
            "[codegen] reclassify-plan: moved {} entry(ies) into aggregates (#790)",
            moves.new_aggregates.len(),
        );
        for a in &moves.new_aggregates {
            eprintln!("  * {}", a.canonical_name);
        }
        ext.aggregates.extend(moves.new_aggregates);
        ext.aggregates
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    if !moves.new_windows.is_empty() {
        eprintln!(
            "[codegen] reclassify-plan: moved {} entry(ies) into window_functions (#790)",
            moves.new_windows.len(),
        );
        for w in &moves.new_windows {
            eprintln!("  * {}", w.canonical_name);
        }
        ext.window_functions.extend(moves.new_windows);
        ext.window_functions
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }
    if !moves.new_scalars.is_empty() {
        eprintln!(
            "[codegen] reclassify-plan: moved {} entry(ies) into scalars (#790)",
            moves.new_scalars.len(),
        );
        for s in &moves.new_scalars {
            eprintln!("  * {}", s.canonical_name);
        }
        ext.scalars.extend(moves.new_scalars);
        ext.scalars
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }

    Ok(())
}

/// #768: routing category the WIT-scalar auto-synthesiser assigns to
/// each upstream WIT function it visits. Detection is cheap and
/// conservative (name-suffix + interface-name + window-shape signature);
/// `classify_aggregate_shape` / `classify_window_shape` remain the
/// authoritative shape-checkers downstream and will unwire mis-routed
/// entries with an informative reason rather than silently emitting
/// wrong code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WitFnCategory {
    Scalar,
    Aggregate,
    Window,
}

/// #768: primary aggregate interfaces — WIT functions declared here
/// are semantically aggregates even without an `-aggregate` / `-agg`
/// suffix (e.g. `postgis-aggregates::st-extent-threed`). Mirrors the
/// `is_primary_agg_interface` list in `build_aggregate_registry`
/// (which routes plan-side aggregate entries to the aggregate
/// dispatcher); keeping the two in step means "aggregate interface"
/// has one meaning across the whole codegen.
fn is_primary_aggregate_interface(iface: &str) -> bool {
    matches!(
        iface,
        "postgis-aggregates" | "postgis-raster-aggregates" | "temporal-aggregate-ops"
    )
}

/// #768: does the WIT function's signature match the window-fn
/// shape (`list<borrow<geometry>>` + `result<list<option<u32>|u32|
/// geometry>>`)? Used as a fallback when the kebab name lacks the
/// `-win` suffix but the signature is unambiguously window-shaped —
/// e.g. `st-cluster-dbscan`, `st-cluster-kmeans`,
/// `st-cluster-kmeans-max-radius` in `postgis-clustering`.
///
/// Kept separate from `classify_window_shape` (which returns a full
/// `WindowShape` with alias / extra-arg classification) — this only
/// needs a yes/no.
fn wit_fn_matches_window_shape(f: &wit_parse::WitFunction) -> bool {
    let Some(first) = f.params.first() else {
        return false;
    };
    if !matches!(first.ty, wit_parse::WitType::ListGeomBorrow) {
        return false;
    }
    match &f.ret.inner {
        wit_parse::WitType::ListOptionU32 => true,
        wit_parse::WitType::ListGeomOwned => true,
        wit_parse::WitType::List(inner) => matches!(inner.as_ref(), wit_parse::WitType::U32),
        _ => false,
    }
}

/// #768: assign one of `Scalar`/`Aggregate`/`Window` to an upstream
/// WIT function so the auto-synthesiser lands it on the right plan
/// surface. Priority order: window (suffix beats shape, but shape
/// alone is enough), aggregate (suffix or primary interface),
/// scalar (default).
///
/// Window is checked before aggregate because a `-win`-suffixed
/// name could otherwise be pulled into the aggregate bucket by a
/// naming coincidence (none observed today, but the ordering
/// keeps future additions honest).
///
/// #782: aggregate detection covers BOTH `-aggregate` / `-agg` at
/// the tail AND `-aggregate-` / `-agg-` as an infix. mobilitydb's
/// temporal-aggregate-ops surface produces names like
/// `int-span-aggregate-union` where `aggregate` sits between two
/// meaningful tokens rather than at the end; the tail-only check
/// mis-routed ~53 fns as scalars.
fn classify_wit_fn_category(f: &wit_parse::WitFunction) -> WitFnCategory {
    // Window: `-win` suffix OR window-shape signature.
    if f.kebab_name.ends_with("-win") || wit_fn_matches_window_shape(f) {
        return WitFnCategory::Window;
    }
    // Aggregate: `-aggregate` / `-agg` suffix, OR `-aggregate-` /
    // `-agg-` infix (#782 — mobilitydb `int-span-aggregate-union`
    // family), OR primary aggregate interface (catches
    // `postgis-aggregates::st-extent-threed` — no explicit suffix).
    if f.kebab_name.ends_with("-aggregate")
        || f.kebab_name.ends_with("-agg")
        || f.kebab_name.contains("-aggregate-")
        || f.kebab_name.contains("-agg-")
        || is_primary_aggregate_interface(&f.interface)
    {
        return WitFnCategory::Aggregate;
    }
    WitFnCategory::Scalar
}

/// #690: return the resource kebab name (`topology`, `raster`) when
/// `f` is a free function in a `<ns>-<resource>-*` family interface
/// AND its first parameter is `borrow<resource>`. This is the
/// signature shape that the OLD hand-written `list_functions()`
/// implementations advertised under a `<resource>_<func>` SQL alias
/// (e.g. `topology_add_iso_node`, `topology_mod_edge_heal`).
///
/// Synthesising the prefixed name keeps the dispatcher's
/// `find_resource_family_free_fn` resolver as the routing path
/// (SQL `topology_add_node` → WIT `add-node` in `postgis-topology-*`).
/// Without the prefix the surface would only expose the bare
/// kebab-snake form (`add_node`), breaking the established
/// `topology_*` naming convention.
///
/// Returns `None` for:
///   - Functions inside a `resource NAME { ... }` block — those
///     are routed via `index_resource_methods`, not this path.
///   - Functions whose first parameter is not `borrow<resource>`.
///   - Constructors (their `<resource>` surface name is
///     `create-<resource>` per #556 W3.1).
fn resource_family_prefix_for(f: &wit_parse::WitFunction) -> Option<&'static str> {
    if f.resource.is_some() || f.is_constructor {
        return None;
    }
    let first = f.params.first()?;
    match first.ty {
        wit_parse::WitType::Topology { borrowed: true } => Some("topology"),
        wit_parse::WitType::Raster { borrowed: true } => Some("raster"),
        _ => None,
    }
}

/// One aggregate dispatch arm. `sql_name` is the canonical SQL
/// name; `aliases` lists any extra names the SQL surface exposes
/// for the same upstream WIT function. Phase 1A (formerly each
/// alias was its own `AggregateEntry`): downstream emitters expand
/// aliases inline at the use site so a single classifier pass
/// drives both the canonical and per-alias dispatch surfaces
/// without forcing the emit layer to dedupe duplicate entries.
pub struct AggregateEntry {
    pub sql_name: String,
    pub shape: AggregateShape,
    pub aliases: Vec<String>,
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
/// accumulator. `Geom` is the original postgis-aggregates surface;
/// `Raster` was added for `st-rast-union-aggregate` and any future
/// `list<borrow<raster>>`-taking aggregate.
///
/// #607 Phase 0: `Record` is the wit-value variant — the accumulator
/// state carries `Vec<WitValuePayload>` (per-row canonical-CBOR
/// payloads) and finalize decodes via the bridge's per-record codec
/// before calling the upstream aggregator. Mobilitydb temporal-type
/// aggregates (`tfloat-temporal-min`, `tint-temporal-max`,
/// `tbool-temporal-and`, etc.) take owned `list<X-sequence>` and
/// return `option<X-sequence>` — the per-row marshaling needs the
/// record codec rather than `from_wkb` / `from_raster_binary`.
///
/// #612 (OQ1): `Record` now carries `input` + `output` `RecordSpec`s
/// independently so different-input/output aggregates wire cleanly
/// — `tgeompoint-st-extent` (input `tgeompoint-sequence`, output
/// `stbox`) and the six `t*-temporal-count` aggregates (input
/// `t*-sequence`, output `tint-sequence`). Same-record aggregates
/// (Phase 1 pilot scope: `tfloat-temporal-min`, etc.) populate
/// `input == output` with the same RecordSpec — byte-identical to
/// the previous flat-field layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccKind {
    Geom,
    Raster,
    /// #607 Phase 0 + #612 (OQ1): resource-record aggregator.
    ///
    /// `input` carries the upstream record kebab + codec metadata
    /// for the per-row payloads streaming into the accumulator.
    /// `output` carries the record kebab + codec metadata for the
    /// upstream aggregator's return record (wrapped in
    /// `option<...>`). Both must be present in the bridge's
    /// record_registry — the classifier enforces this.
    ///
    /// Same-record aggregates (`tfloat-temporal-min`,
    /// `tbool-temporal-and`, etc.) have `input == output` — the
    /// finalize body's decode + encode sites resolve identical
    /// codec helpers. Different-record aggregates resolve the two
    /// codec sites against distinct records (e.g. decode via
    /// `arg_witvalue_tgeompoint_sequence`, encode via
    /// `ret_to_witvalue_stbox`).
    Record {
        input: RecordSpec,
        output: RecordSpec,
    },
    /// #614: record-typed list input, primitive-scalar output.
    ///
    /// Step body is structurally identical to `Record` (push the
    /// per-row `WitValuePayload` onto the witvalue accumulator
    /// state + latch any extras). Finalize decodes each payload
    /// via the input record's `arg_witvalue_<snake>` helper, calls
    /// the upstream aggregator, and wraps the primitive return in
    /// the target's native scalar variant (`SqlValue::Integer`,
    /// `Duckvalue::Bigint`, `ScalarValue::UInt32`, etc.) selected
    /// by `output`.
    ///
    /// Today's surface (mobilitydb): the three trajectory-pattern
    /// counters `tgeompoint-num-convoys` / `-flocks` / `-meetings`
    /// take `list<tgeompoint-sequence>` + a handful of f64/s64/u32
    /// extras and return `u32`. They didn't fit `Geom`/`Raster`
    /// (raw-blob accumulator) or `Record` (record-out finalize)
    /// before this kind landed.
    ///
    /// #637: `optional = true` covers `func(list<X-sequence>) -> option<T>`
    /// where T is a primitive matching `ScalarReturnKind`. Today's
    /// surface (mobilitydb): `tint-min-value-aggregate` /
    /// `tint-max-value-aggregate` return `option<s64>` — the
    /// finalize encoder wraps `Some(v)` per the primitive variant
    /// and emits the target's native NULL on `None`.
    RecordToScalar {
        input: RecordSpec,
        output: ScalarReturnKind,
        optional: bool,
    },
    /// #640: record-typed list input, primitive-tuple output.
    ///
    /// Structurally identical to `RecordToScalar` on the input side
    /// (decode each per-row witvalue payload via the per-input-record
    /// `arg_witvalue_<snake>` helper). The output side serialises the
    /// upstream Rust tuple to a JSON-array text via
    /// `serde_json::to_string` (matches the `JsonRetKind::TuplePrim`
    /// / `OptionTuplePrim` pattern used by the scalar return paths) —
    /// each emit target wraps that string in its native Text / Utf8
    /// variant.
    ///
    /// `optional = true` covers `func(list<X-sequence>) -> option<tuple<T,U,...>>`
    /// where each Ti is a primitive matching `ScalarReturnKind`. Today's
    /// surface (mobilitydb): `tint-range-aggregate` returns
    /// `option<tuple<s64, s64>>` — finalize emits the target's native
    /// NULL on `None`, JSON-array text on `Some(t)`.
    ///
    /// `output` carries one `ScalarReturnKind` per tuple element. The
    /// Vec layout (rather than `output_a` / `output_b` pair) mirrors
    /// `JsonRetKind::TuplePrim(Vec<ListPrimElem>)` and generalises
    /// trivially to n-element tuples — the emit body is the same
    /// `serde_json::to_string` shape regardless of arity.
    RecordToTuple {
        input: RecordSpec,
        output: Vec<ScalarReturnKind>,
        optional: bool,
    },
    /// #799: nested-list-of-record input, list-of-record output.
    ///
    /// Each row streams a full spanset — a `list<record>` — and the
    /// finalize call takes `list<list<record>>`, returning a unified
    /// `list<record>` (bare, no `option<>` wrapper). Distinct from
    /// `Record` on both sides:
    ///   - Step body pushes ONE `Vec<UPSTREAM>` (serialized per row)
    ///     onto the accumulator state rather than a single UPSTREAM
    ///     payload — the input is a set, not a single element.
    ///   - Finalize decodes the accumulator into `Vec<Vec<UPSTREAM>>`
    ///     and hands it to the upstream aggregator as `&[Vec<R>]`.
    ///   - Output side wraps the full `Vec<R>` back into a JSON-array
    ///     text — SQL callers unpack via `json_each` / DuckDB's
    ///     `json_extract`. This preserves the whole unified spanset
    ///     rather than the FirstWitValueRecord "first element only"
    ///     collapse used elsewhere.
    ///
    /// Today's surface (mobilitydb `span-union-ops`): the four
    /// `<int|float|date|tstz>-spanset-aggregate-union` fns.
    RecordSetToRecordSet {
        input: RecordSpec,
        output: RecordSpec,
    },
    /// #830: record-typed list input, primitive-list output.
    ///
    /// Structurally identical to `RecordToScalar` on the input side
    /// (decode each per-row witvalue payload via the per-input-record
    /// `arg_witvalue_<snake>` helper). The output side serialises the
    /// upstream Rust `Vec<T>` (for primitive `T`) to a JSON-array text
    /// via `serde_json::to_string` — same emit template as
    /// `RecordToTuple` and `JsonRetKind::ListListPrim`. Each emit
    /// target wraps the string in its native Text / Utf8 variant.
    ///
    /// `output` carries the `ListPrimElem` for the vec element type.
    /// Serde-derives auto-implement `Serialize` for `Vec<T>` of any
    /// primitive T, so the emit body is uniform — the element kind
    /// is captured for diagnostics / future typed rendering.
    ///
    /// Today's surface (mobilitydb `temporal-aggregate-ops`):
    /// `tjsonb-sequences-agg-collect-keys(list<tjsonb-sequence>)
    /// -> list<string>`. Extends naturally to any
    /// `list<record> -> list<primitive>` aggregate.
    RecordToListPrim {
        input: RecordSpec,
        output: ListPrimElem,
    },
}

/// #614: primitive scalar return for `AccKind::RecordToScalar`.
///
/// The `classify_return` path collapses every integer width to
/// `RetShape::Int` / every float width to `RetShape::Real` /
/// `bool` to `RetShape::BoolInt`; for record-input aggregates we
/// re-walk the raw `WitType` so each emit target can pick the
/// precise native-scalar wrap on the finalize encoder rather than
/// going through the collapsed RetShape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScalarReturnKind {
    U32,
    S32,
    U64,
    S64,
    U8,
    F64,
    F32,
    Bool,
}

/// #612 (OQ1): canonical per-record identifying tuple. The six
/// fields are exactly what the bridge's per-record codec helpers
/// (`arg_witvalue_<snake>` / `ret_to_witvalue_<snake>`) reference,
/// plus what `typed-value-binding` uses for host-side dispatch.
///
/// Carried by `AccKind::Record` (input + output) so different-record
/// aggregates resolve the two codec sites independently. Mirrors
/// the field shape of `RetShape::OptionWitValueRecord` /
/// `RetShape::WitValueRecord` for cross-form reuse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordSpec {
    pub kebab_name: String,
    pub wit_interface: String,
    pub wit_package: String,
    pub wit_package_version: String,
    pub symbolic_name: String,
    pub type_id_hex: String,
    /// #710: helper-function suffix — see `ParamShape::WitValueRecord::helper_snake`.
    pub helper_snake: String,
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
        // Gap G3 (#668): bbox3d renders as an ISO-WKB
        // `LINESTRING Z` blob via `RetShape::Bbox3dWkbLineZ`, so
        // its column affinity matches the 2D `Bbox` form (Blob).
        WitType::Bbox3d => ColumnAffinity::Blob,
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
        let ps = classify_param(&p.ty, records, enums, &f.interface).map_err(|why| {
            format!(
                "param #{i} ({:?}: {:?}) not wired: {why}",
                p.name, p.ty
            )
        })?;
        params.push(ps);
    }

    let ret = classify_return(&f.ret, records, enums, &f.interface)?;

    Ok(DispatchShape {
        wit_module: alias,
        wit_package: f.package.clone(),
        wit_func,
        params,
        ret,
        method_call,
    })
}

/// #614: map a raw return `WitType` to the precise scalar primitive
/// kind for `AccKind::RecordToScalar`. Returns `None` if the WIT
/// shape isn't a primitive — record/option/list/etc. all fall
/// through, and the caller falls back to the existing failure
/// branch.
///
/// Mirrors the prim-arm subset of `classify_return` but keeps the
/// integer widths distinct so each emit target can pick the right
/// native-scalar wrap on the finalize encoder.
///
/// #830: `list_prim_return_kind` is the sibling helper for
/// `AccKind::RecordToListPrim` — it walks the same primitive alphabet
/// but includes `string` (which `ScalarReturnKind` intentionally
/// omits, since the SQLite scalar wrap has no bare-Text arm for it).
/// `list<string>` is a legitimate aggregate return shape (mobilitydb
/// `tjsonb-sequences-agg-collect-keys`) — the emit body serialises the
/// whole `Vec<String>` as a JSON-array text.
fn list_prim_return_kind(t: &WitType) -> Option<ListPrimElem> {
    match t {
        WitType::U32 => Some(ListPrimElem::U32),
        WitType::S32 => Some(ListPrimElem::S32),
        WitType::U64 => Some(ListPrimElem::U64),
        WitType::S64 => Some(ListPrimElem::S64),
        WitType::U8 => Some(ListPrimElem::U8),
        WitType::F64 => Some(ListPrimElem::F64),
        WitType::F32 => Some(ListPrimElem::F32),
        WitType::Bool => Some(ListPrimElem::Bool),
        WitType::String => Some(ListPrimElem::String),
        _ => None,
    }
}

fn scalar_return_kind(t: &WitType) -> Option<ScalarReturnKind> {
    match t {
        WitType::U32 => Some(ScalarReturnKind::U32),
        WitType::S32 => Some(ScalarReturnKind::S32),
        WitType::U64 => Some(ScalarReturnKind::U64),
        WitType::S64 => Some(ScalarReturnKind::S64),
        WitType::U8 => Some(ScalarReturnKind::U8),
        WitType::F64 => Some(ScalarReturnKind::F64),
        WitType::F32 => Some(ScalarReturnKind::F32),
        WitType::Bool => Some(ScalarReturnKind::Bool),
        _ => None,
    }
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
    // type or an owned-list of a record type. #548 (W3.2): both
    // `list<borrow<geometry>>` (Geom) and `list<borrow<raster>>`
    // (Raster) are accepted; the kind picks the per-context
    // accumulator helpers + the finalize decoder. #607 Phase 0:
    // also accept `list<X>` where `X` is a record-typed kebab in
    // the bridge's record_registry (mobilitydb temporal-type
    // aggregates).
    if f.params.is_empty() {
        return Err("aggregate has zero params".into());
    }
    let first = &f.params[0].ty;
    // #607 Phase 0: `list<X>` owned where X is a record kebab —
    // temporal-aggregate-ops shape. The classifier also resolves
    // the output record from the return shape so the AccKind
    // carries both halves of a possibly-asymmetric in/out pair
    // (`tgeompoint-st-extent` returns `option<stbox>`; the six
    // `t*-temporal-count` aggregates return `option<tint-sequence>`).
    // #799: `input_is_nested` distinguishes the flat `list<record>`
    // shape (Phase 1 pilot: mobilitydb temporal-type aggregates —
    // input rows carry one record each) from the nested
    // `list<list<record>>` shape (`<int|float|date|tstz>-spanset-
    // aggregate-union` — input rows carry a spanset = list of
    // records). The nested shape drives the new
    // `AccKind::RecordSetToRecordSet` variant below.
    let mut input_is_nested = false;
    let input_spec: Option<RecordSpec> = match first {
        WitType::List(inner) => match inner.as_ref() {
            WitType::Unsupported(name) => {
                if let Some(rec) = find_record(records, name, &f.interface) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    Some(RecordSpec {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                        helper_snake: rec.helper_snake(),
                    })
                } else {
                    return Err(format!(
                        "first aggregate param `list<{}>` has no matching record in the bridge registry",
                        name,
                    ));
                }
            }
            // #799: `list<list<record>>` — nested aggregate input.
            // Each row streams a full spanset (list<X-span>); the
            // finalize call takes `list<list<X-span>>` and returns a
            // unified spanset. Today's surface: mobilitydb
            // `span-union-ops`'s four `<int|float|date|tstz>-spanset-
            // aggregate-union` fns.
            WitType::List(inner2) => match inner2.as_ref() {
                WitType::Unsupported(name) => {
                    if let Some(rec) = find_record(records, name, &f.interface) {
                        let type_id_hex: String =
                            rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                        input_is_nested = true;
                        Some(RecordSpec {
                            kebab_name: rec.kebab_name.clone(),
                            wit_interface: rec.interface.clone(),
                            wit_package: rec.package.clone(),
                            wit_package_version: rec.package_version.clone(),
                            symbolic_name: rec.symbolic_name.clone(),
                            type_id_hex,
                            helper_snake: rec.helper_snake(),
                        })
                    } else {
                        return Err(format!(
                            "first aggregate param `list<list<{}>>` has no matching record in the bridge registry",
                            name,
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "first aggregate param must be list<borrow<geometry>>, list<borrow<raster>>, list<record>, or list<list<record>>; got list<list<{:?}>>",
                        inner2,
                    ));
                }
            },
            _ => {
                return Err(format!(
                    "first aggregate param must be list<borrow<geometry>>, list<borrow<raster>>, list<record>, or list<list<record>>; got list<{:?}>",
                    inner,
                ));
            }
        },
        WitType::ListGeomBorrow | WitType::ListRasterBorrow => None,
        other => {
            return Err(format!(
                "first aggregate param must be list<borrow<geometry>>, list<borrow<raster>>, list<record>, or list<list<record>>; got {:?}",
                other,
            ));
        }
    };
    let accumulator_kind_partial = match first {
        WitType::ListGeomBorrow => Some(AccKind::Geom),
        WitType::ListRasterBorrow => Some(AccKind::Raster),
        _ => None, // Record: built below once we resolve the output
    };

    // Subsequent params (st-cluster-within takes f64 distance, etc.)
    let mut extra = Vec::new();
    for (i, p) in f.params.iter().enumerate().skip(1) {
        extra.push(classify_param(&p.ty, records, enums, &f.interface).map_err(|why| {
            format!("aggregate extra param #{i}: {why}")
        })?);
    }

    let ret = classify_return(&f.ret, records, enums, &f.interface)?;

    // #607 Phase 0 + #612 (OQ1): when the first param is a
    // `list<record>` (Record accumulator path), resolve the output
    // record from the return shape and build the AccKind::Record
    // pair. Same-record aggregates have `input == output` (Phase 1
    // pilot scope); different-record aggregates carry distinct
    // specs (#612: `tgeompoint-st-extent`, `t*-temporal-count`).
    //
    // Output record sources:
    //  - `option<R>` return (`OptionWitValueRecord`) — the canonical
    //    mobilitydb shape.
    //  - bare `R` (`WitValueRecord`) — supported for symmetry; no
    //    known aggregate uses it today.
    //  - `list<R>` first-element projection (`FirstWitValueRecord`)
    //    — same.
    //
    // Anything else (primitive return, non-record option) is
    // rejected: the finalize encoder needs a record codec on the
    // output side.
    let accumulator_kind = match (accumulator_kind_partial, input_spec) {
        (Some(k), _) => k, // Geom / Raster: no output-record resolution
        (None, Some(input)) => {
            // Record-typed input: branch on whether the return is
            // another record (the existing `AccKind::Record` path)
            // or a primitive scalar (#614 `RecordToScalar`).
            let output_kebab = match &ret {
                RetShape::OptionWitValueRecord { kebab_name, .. }
                | RetShape::WitValueRecord { kebab_name, .. }
                | RetShape::FirstWitValueRecord { kebab_name, .. } => Some(kebab_name.as_str()),
                _ => None,
            };
            if let Some(out_kebab) = output_kebab {
                let output = if out_kebab == input.kebab_name.as_str() {
                    input.clone()
                } else if let Some(rec) = find_record(records, out_kebab, &f.interface) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    RecordSpec {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                        helper_snake: rec.helper_snake(),
                    }
                } else {
                    return Err(format!(
                        "AccKind::Record aggregate output `{}` has no matching record in the bridge registry",
                        out_kebab,
                    ));
                };
                // #799: nested-list input (`list<list<record>>`) + a
                // record-shaped return picks the RecordSetToRecordSet
                // variant so the finalize path preserves the whole
                // output list rather than collapsing to a first-
                // element projection.
                if input_is_nested {
                    AccKind::RecordSetToRecordSet { input, output }
                } else {
                    AccKind::Record { input, output }
                }
            } else if let Some(scalar_out) = scalar_return_kind(&f.ret.inner) {
                // #614: list<record> → primitive scalar shape.
                // RetShape collapses integer widths (every
                // u32/s32/u64/etc. lands on RetShape::Int) so re-
                // walk the raw WitType to capture the precise
                // width.
                AccKind::RecordToScalar {
                    input,
                    output: scalar_out,
                    optional: false,
                }
            } else if let WitType::Option(inner) = &f.ret.inner {
                // #637: list<record> → option<primitive> scalar.
                // Mirrors the bare-scalar arm above but the
                // finalize encoder wraps the upstream return in
                // `Some(v) → target-native scalar variant` /
                // `None → target-native NULL`. Today's surface:
                // mobilitydb `tint-min-value-aggregate` /
                // `tint-max-value-aggregate` returning `option<s64>`.
                if let Some(scalar_out) = scalar_return_kind(inner) {
                    AccKind::RecordToScalar {
                        input,
                        output: scalar_out,
                        optional: true,
                    }
                } else if let WitType::Tuple(elems) = inner.as_ref() {
                    // #640: list<record> → option<tuple<T,U,...>>
                    // over primitives. Each tuple element must
                    // match `ScalarReturnKind`; the emit body
                    // serialises the upstream Rust tuple via
                    // `serde_json::to_string` and wraps the result
                    // in the target's Text / Utf8 variant
                    // (`Some(t)` → JSON-array text, `None` →
                    // native NULL). Today's surface (mobilitydb):
                    // `tint-range-aggregate` returns
                    // `option<tuple<s64, s64>>`.
                    let outs: Option<Vec<ScalarReturnKind>> =
                        elems.iter().map(scalar_return_kind).collect();
                    if let Some(outs) = outs {
                        if outs.is_empty() {
                            return Err(format!(
                                "AccKind::Record aggregate input `{}` but return shape is option<tuple<>> (empty tuple not supported)",
                                input.kebab_name,
                            ));
                        }
                        AccKind::RecordToTuple {
                            input,
                            output: outs,
                            optional: true,
                        }
                    } else {
                        return Err(format!(
                            "AccKind::Record aggregate input `{}` but return shape is option<tuple<{:?}>> (tuple element not a recognised primitive scalar)",
                            input.kebab_name,
                            elems,
                        ));
                    }
                } else {
                    return Err(format!(
                        "AccKind::Record aggregate input `{}` but return shape is option<{:?}> (inner not a recognised primitive scalar)",
                        input.kebab_name,
                        inner,
                    ));
                }
            } else if let WitType::Tuple(elems) = &f.ret.inner {
                // #640: list<record> → tuple<T,U,...> (bare) over
                // primitives. Symmetric with the optional arm
                // above; no known mobilitydb aggregate uses this
                // bare shape today, but the variant is wired so
                // a future entry doesn't need a classifier patch.
                let outs: Option<Vec<ScalarReturnKind>> =
                    elems.iter().map(scalar_return_kind).collect();
                if let Some(outs) = outs {
                    if outs.is_empty() {
                        return Err(format!(
                            "AccKind::Record aggregate input `{}` but return shape is tuple<> (empty tuple not supported)",
                            input.kebab_name,
                        ));
                    }
                    AccKind::RecordToTuple {
                        input,
                        output: outs,
                        optional: false,
                    }
                } else {
                    return Err(format!(
                        "AccKind::Record aggregate input `{}` but return shape is tuple<{:?}> (tuple element not a recognised primitive scalar)",
                        input.kebab_name,
                        elems,
                    ));
                }
            } else if let WitType::List(inner) = &f.ret.inner {
                // #830: list<record> → list<primitive>. Mirrors the
                // `list<record>` return shape (#795 landed
                // `RecordSetToRecordSet` for the record case) but the
                // output side serialises `Vec<T>` for a primitive T.
                // Today's surface (mobilitydb):
                // `tjsonb-sequences-agg-collect-keys(list<tjsonb-sequence>)
                //   -> list<string>`.
                // Extends to any `list<record> -> list<primitive>` shape.
                if let Some(elem) = list_prim_return_kind(inner) {
                    AccKind::RecordToListPrim {
                        input,
                        output: elem,
                    }
                } else {
                    return Err(format!(
                        "AccKind::Record aggregate input `{}` but return shape is list<{:?}> (list element not a recognised primitive scalar)",
                        input.kebab_name,
                        inner,
                    ));
                }
            } else {
                return Err(format!(
                    "AccKind::Record aggregate input `{}` but return shape is not a record or recognised primitive scalar",
                    input.kebab_name,
                ));
            }
        }
        (None, None) => unreachable!("classifier above rejects non-list-record first params"),
    };

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
        params.push(classify_param(&p.ty, records, enums, &f.interface).map_err(|why| {
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
    // #753 / #754: normalize the return type-tree so a `record
    // geography` (mdb tgeography surface) or `record geometry` (mdb
    // tgeometry surface) routes through the record path instead of
    // the postgis resource path — see `classify_param` /
    // `classify_return` for the parallel treatment on scalar shapes.
    let ret_normalized = normalize_ambiguous_records(&f.ret.inner, records, &f.interface);
    let output_row = classify_udtf_output_row(&ret_normalized, records, aliases);
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
/// #710: look up a record by kebab, preferring the one declared in
/// the caller's own interface. Two shims (mobilitydb-temporal today,
/// potentially others) declare records with the same kebab in two
/// different interfaces (`stbox3d` lives in both `stbox-ops` and
/// `stbox3d-ops`, with different field orders). Each function
/// references the record from its own interface's scope, so the
/// classifier must prefer the local-to-caller record over any
/// cross-interface one that happens to share the kebab.
///
/// Fallback order: same-interface, then any interface. When there's
/// no collision, this reduces to the pre-#710 `iter().find` lookup.
fn find_record<'a>(
    records: &'a [RecordType],
    kebab: &str,
    caller_interface: &str,
) -> Option<&'a RecordType> {
    records
        .iter()
        .find(|r| r.kebab_name == kebab && r.interface == caller_interface)
        .or_else(|| records.iter().find(|r| r.kebab_name == kebab))
}

/// #753 / #754: resource-vs-record kebab collisions.
///
/// `wit_parse::parse_type` eagerly promotes a handful of bare kebabs
/// (`"geometry"`, `"geography"`, ...) to postgis-style *resource*
/// variants (`WitType::Geometry`, `WitType::Geography`) without
/// consulting the record registry. That's the right call for
/// postgis-* bridges — the resource form routes through the
/// `.as_wkb()` / `geom_from_wkb` / `geog_from_wkb` dispatch shape.
///
/// mobilitydb-wasm shims collide on the same bare kebabs:
///   - #753 (mdb #739): `record geography { srid, kind, point, wkb }`
///     in `tgeography-ops` — WKB-carrying record.
///   - #754 (mdb #740): `record geometry { srid, kind, point, coords,
///     rings }` in `tgeometry-ops` — STRUCTURAL record (no `wkb` field;
///     point / coords / rings carry the shape).
///
/// The record has no `.as_wkb()` method and the postgis-only
/// `geom_from_wkb` / `geog_from_wkb` helpers aren't emitted for the
/// mdb bridge — so classifying via the resource path leaves the
/// codegen with dangling method + function references.
///
/// Fix: when the caller_interface's record registry defines a same-
/// kebab record, rewrite the resource variant down to
/// `WitType::Unsupported(kebab)` so the downstream Unsupported arms
/// route through `find_record` → WitValueRecord (same treatment as
/// every other record-typed param/return). The per-record codec
/// helpers (`arg_witvalue_<snake>` / `ret_to_witvalue_<snake>`) are
/// ciborium-based and structure-agnostic, so they cover both the
/// wkb-carrying (#753) and structural (#754) field layouts without
/// per-record hand-emitted code.
///
/// The `list<geometry>` return shape has its own eager promotion
/// (`WitType::ListGeomOwned`) — rewrite it to
/// `List<Unsupported("geometry")>` so `RetShape::FirstWitValueRecord`
/// picks it up (mobilitydb-wasm `tgeometry-values` return).
///
/// The walk covers `Option<T>`, `List<T>`, `Result<Ok, Err>` and
/// `Tuple<..., T, ...>` so every nested context sees the record path
/// uniformly. Postgis stays on the resource path — `find_record`
/// returns `None` when no matching record exists, so the walk is a
/// no-op for postgis-sqlink-bridge / postgis-duckdb-bridge /
/// postgis-datafission regen.
///
/// Same-name collisions on other postgis resources (`raster`,
/// `topology`) aren't triggered by any mdb WIT today; they'd fall
/// through the existing branch unchanged. To extend, add another
/// `WitType::Raster { .. } if has_raster => ..` arm.
fn normalize_ambiguous_records(
    t: &WitType,
    records: &[RecordType],
    caller_interface: &str,
) -> WitType {
    let has_geog = find_record(records, "geography", caller_interface).is_some();
    let has_geom = find_record(records, "geometry", caller_interface).is_some();
    if !has_geog && !has_geom {
        return t.clone();
    }
    fn walk(t: &WitType, has_geog: bool, has_geom: bool) -> WitType {
        match t {
            WitType::Geography { .. } if has_geog => {
                WitType::Unsupported("geography".to_string())
            }
            WitType::Geometry { .. } if has_geom => {
                WitType::Unsupported("geometry".to_string())
            }
            // `list<geometry>` is eagerly promoted to `ListGeomOwned`
            // by `wit_parse::parse_type`; unfold it here so
            // `RetShape::FirstWitValueRecord` / `ParamShape::ListRecord`
            // routing catches it via the standard `List<Unsupported>`
            // classification path.
            WitType::ListGeomOwned if has_geom => WitType::List(Box::new(
                WitType::Unsupported("geometry".to_string()),
            )),
            WitType::ListGeomBorrow if has_geom => WitType::List(Box::new(
                WitType::Unsupported("geometry".to_string()),
            )),
            WitType::Option(inner) => {
                WitType::Option(Box::new(walk(inner, has_geog, has_geom)))
            }
            WitType::List(inner) => WitType::List(Box::new(walk(inner, has_geog, has_geom))),
            WitType::Result(ok, err) => WitType::Result(
                Box::new(walk(ok, has_geog, has_geom)),
                Box::new(walk(err, has_geog, has_geom)),
            ),
            WitType::Tuple(elems) => WitType::Tuple(
                elems.iter().map(|e| walk(e, has_geog, has_geom)).collect(),
            ),
            other => other.clone(),
        }
    }
    walk(t, has_geog, has_geom)
}

/// #716: same-kebab enum disambiguation — mirror of `find_record`.
/// `trend-direction` is declared in BOTH `pattern-ops` and
/// `statistics-ops` in the mobilitydb WIT (identical case list, but
/// wit-bindgen emits two distinct Rust types). Fall through to the
/// caller's interface first so `statistics_ops::TrendDirection` matches
/// a `statistics-ops` function's return; otherwise fall back to any
/// matching kebab.
fn find_enum<'a>(
    enums: &'a [EnumWithPackage],
    kebab: &str,
    caller_interface: &str,
) -> Option<&'a EnumWithPackage> {
    enums
        .iter()
        .find(|e| e.decl.kebab_name == kebab && e.decl.interface == caller_interface)
        .or_else(|| enums.iter().find(|e| e.decl.kebab_name == kebab))
}

pub fn classify_param(
    t: &WitType,
    records: &[RecordType],
    enums: &[EnumWithPackage],
    caller_interface: &str,
) -> Result<ParamShape, String> {
    // #753 / #754: rewrite same-kebab collisions from resource-form
    // (`WitType::Geography` / `WitType::Geometry`) to record-form
    // (`WitType::Unsupported(kebab)`) when the caller's package
    // declares a same-name record. See `normalize_ambiguous_records`
    // for the disambiguation rationale.
    let normalized = normalize_ambiguous_records(t, records, caller_interface);
    let t = &normalized;
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
        WitType::Bbox3d => {
            // Round (#608): bbox3d only appears as a return today
            // (`st-extent-threed`); reject in param position so a
            // future shape that takes it surfaces a named diagnostic.
            return Err(format!(
                "param type not in dispatcher alphabet: bbox3d (returns only)"
            ));
        }
        WitType::List(inner) => {
            // #674: `list<list<u8>>` — batched WKB blobs surfaced by
            // postgis's `st_*_batch` family. SQL passes JSON-text
            // matching `Vec<Vec<u8>>` (nested integer arrays); the
            // dispatch arm decodes via a codegen-emitted helper.
            // Take precedence over the generic primitive path since
            // `ListU8` doesn't appear in `list_prim_elem`.
            if matches!(inner.as_ref(), WitType::ListU8) {
                return Ok(ParamShape::ListListU8);
            }
            // #695: `list<list<X>>` for primitive non-u8 elements
            // (`list<list<f64>>` is on the postgis raster surface
            // via `st-set-values`; flatgeobuf has it for
            // `make-polygon-with-holes` / `make-multilinestring`).
            // Sits before the `ListPrim` check so the nested-list
            // path wins over a misclassification as a flat list of
            // an unsupported inner type.
            if let WitType::List(inner_inner) = inner.as_ref() {
                if let Some(elem) = list_prim_elem(inner_inner) {
                    return Ok(ParamShape::ListListPrim(elem));
                }
                // #781: `list<list<R>>` where R is a same-shim
                // record. Today's scalar surface (mobilitydb):
                // the spanset-extent-ops interface's four
                // `<int|float|date|tstz>-spanset-extent` fns take
                // `list<list<{int,float,date}-span>>`. Records
                // arrive as `WitType::Unsupported(kebab)`; look up
                // in the registry via `find_record` (respects
                // caller_interface for same-kebab collisions,
                // parallel to how `ListRecord` handles the flat
                // case). The sibling `spanset-aggregate-union`
                // family routes through `classify_aggregate_shape`
                // on the aggregate path (see #782 for the
                // `-aggregate-` infix detection) so it doesn't
                // reach this scalar-param arm.
                if let WitType::Unsupported(name) = inner_inner.as_ref() {
                    if let Some(rec) = find_record(records, name, caller_interface) {
                        return Ok(ParamShape::ListListRecord {
                            kebab_name: rec.kebab_name.clone(),
                            wit_interface: rec.interface.clone(),
                            wit_package: rec.package.clone(),
                            wit_package_version: rec.package_version.clone(),
                            helper_snake: rec.helper_snake(),
                        });
                    }
                }
            }
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
                if let Some(rec) = find_record(records, name, caller_interface) {
                    return Ok(ParamShape::ListRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        helper_snake: rec.helper_snake(),
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
                // #724: mixed tuple with at least one same-shim record
                // element. `tfloat-batch-to-parquet` param is
                // `list<tuple<string, tfloat-sequence>>`. Records fall
                // through as `WitType::Unsupported(kebab)`; look them up
                // in the registry via `find_record`, preserving the
                // #709 caller-interface preference for same-kebab
                // collisions.
                let mixed: Option<Vec<ListTupleElem>> = elems
                    .iter()
                    .map(|e| {
                        if let Some(p) = list_prim_elem(e) {
                            return Some(ListTupleElem::Prim(p));
                        }
                        if let WitType::Unsupported(kebab) = e {
                            if let Some(rec) = find_record(records, kebab, caller_interface) {
                                return Some(ListTupleElem::Record(TupleRecordRef {
                                    kebab_name: rec.kebab_name.clone(),
                                    wit_interface: rec.interface.clone(),
                                    wit_package: rec.package.clone(),
                                    wit_package_version: rec.package_version.clone(),
                                    helper_snake: rec.helper_snake(),
                                }));
                            }
                        }
                        None
                    })
                    .collect();
                if let Some(mixed) = mixed {
                    if !mixed.is_empty()
                        && mixed
                            .iter()
                            .any(|e| matches!(e, ListTupleElem::Record(_)))
                    {
                        return Ok(ParamShape::ListTupleMixed { elements: mixed });
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
            if let Some(en) = find_enum(enums, s, caller_interface) {
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
            if let Some(rec) = find_record(records, s, caller_interface) {
                return Ok(ParamShape::WitValueRecord {
                    kebab_name: rec.kebab_name.clone(),
                    wit_interface: rec.interface.clone(),
                    wit_package: rec.package.clone(),
                    wit_package_version: rec.package_version.clone(),
                    upstream_by_value: rec.is_copy,
                    helper_snake: rec.helper_snake(),
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
    caller_interface: &str,
) -> Result<RetShape, String> {
    // #753 / #754: rewrite same-kebab collisions (`geography`,
    // `geometry`) from resource-form to record-form when the caller's
    // package declares a same-name record. See
    // `normalize_ambiguous_records` for rationale. Applied to
    // `r.inner` before every downstream match so bare / `option<>` /
    // `list<>` wrappers all see the record-path routing.
    let inner_normalized = normalize_ambiguous_records(&r.inner, records, caller_interface);
    // #690: `result<_, E>` (unit OK) — parsed as
    // `WitType::Unsupported("_")` by `wit_parse::parse_type`. Map to
    // `RetShape::Unit` BEFORE the generic `Unsupported(_)` arm so
    // mutator-style functions (topology `remove-iso-node`,
    // `change-edge-geom`, gdal `set-projection`, etc.) get a working
    // dispatch arm that returns `SqlValue::Null` on success.
    if let WitType::Unsupported(s) = &inner_normalized {
        if s == "_" {
            return Ok(RetShape::Unit);
        }
    }
    Ok(match &inner_normalized {
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
                // #716: option<tuple<...>> where some elements are
                // `option<primitive>` (bare primitives + optional
                // primitives mixed). serde renders Rust `Option<X>`
                // fields as JSON `null` on the None side, so this
                // reuses the same "encode via serde_json" template
                // as `OptionTuplePrim`. Covers mobilitydb
                // `parse-wkb-point -> option<tuple<f64, f64, option<u32>>>`.
                let mixed: Option<Vec<TupleElemKind>> = elems
                    .iter()
                    .map(|e| match e {
                        WitType::Option(inner) => list_prim_elem(inner)
                            .map(TupleElemKind::OptionPrim),
                        other => list_prim_elem(other).map(TupleElemKind::Prim),
                    })
                    .collect();
                if let Some(mixed) = mixed {
                    if !mixed.is_empty() {
                        return Ok(RetShape::JsonText {
                            kind: JsonRetKind::OptionTuplePrimOrOptPrim(mixed),
                        });
                    }
                }
                let parts: Vec<String> = elems.iter().map(type_label_dbg).collect();
                return Err(format!(
                    "return type not in dispatcher alphabet: option<tuple<{}>> (tuple shape not yet wired)",
                    parts.join(", ")
                ));
            }
            // #630: option<list<R>> where R is a record with
            // all-primitive fields. Some → JSON array of objects via
            // serde; None → SQL NULL. Mobilitydb surface today:
            // `<date|float|int|tstz>-spanset-from-text`. The
            // record-of-primitives constraint keeps the variant from
            // silently expanding to records that nest other records
            // (which `serde_json::to_string` would still encode but
            // SQL callers can't unpack via simple JSON1 ops without
            // matching the nested shape).
            WitType::List(inner_list) => {
                // #716: option<list<primitive>> — Vec<X> serialized via
                // serde. Covers the mobilitydb `*-set-from-text`
                // constructors (`date-set-from-text -> option<list<s32>>`,
                // etc.).
                if let Some(elem) = list_prim_elem(inner_list) {
                    return Ok(RetShape::JsonText {
                        kind: JsonRetKind::OptionListPrim(elem),
                    });
                }
                // #716: option<list<tuple<primitive...>>> — Vec<(X1,X2,...)>
                // serialized via serde. Covers mobilitydb
                // `parse-geojson-linestring -> option<list<tuple<f64, f64>>>`.
                if let WitType::Tuple(elems) = inner_list.as_ref() {
                    let prims: Option<Vec<ListPrimElem>> =
                        elems.iter().map(list_prim_elem).collect();
                    if let Some(prims) = prims {
                        if !prims.is_empty() {
                            return Ok(RetShape::JsonText {
                                kind: JsonRetKind::OptionListTuplePrim(prims),
                            });
                        }
                    }
                }
                if let WitType::Unsupported(rec_kebab) = inner_list.as_ref() {
                    if let Some(rec) =
                        find_record(records, rec_kebab, caller_interface)
                    {
                        if record_fields_all_primitive(rec) {
                            return Ok(RetShape::JsonText {
                                kind: JsonRetKind::OptionListPrimRecord(
                                    rec.kebab_name.clone(),
                                ),
                            });
                        }
                        // #799: record with nested compound fields
                        // (e.g. `list<record>` — mobilitydb
                        // `t*-append-sequence` returns
                        // `option<list<t*-sequence>>` where
                        // `t*-sequence` carries `instants:
                        // list<t*-instant>`). Emits the same
                        // `serde_json::to_string(&Vec<R>)` template
                        // as `OptionListPrimRecord` — wit-bindgen's
                        // `additional_derives` supplies `Serialize`
                        // on every record (transitively), so nested
                        // record fields render as nested JSON
                        // objects.
                        return Ok(RetShape::JsonText {
                            kind: JsonRetKind::OptionListRecord(
                                rec.kebab_name.clone(),
                            ),
                        });
                    }
                }
                return Err(format!(
                    "return type not in dispatcher alphabet: option<list<{}>> (inner not a matching record)",
                    type_label_dbg(inner_list)
                ));
            }
            // Phase F (#522): option<record>. Inner unsupported(name)
            // hits the record registry; if found, route to
            // `OptionWitValueRecord` — Some(rec)→wit-value,
            // None→Null.
            WitType::Unsupported(s) => {
                // #716: option<enum> — Some(variant) → integer
                // discriminant; None → SQL NULL. Check enums BEFORE
                // records because the enum registry supersedes the
                // record registry when both would match (no overlap
                // in the mobilitydb surface today; future-proof).
                if let Some(en) = find_enum(enums, s, caller_interface) {
                    let wit_module =
                        wit_parse::interface_to_rust_alias(&en.decl.interface).ok_or_else(|| {
                            format!(
                                "enum '{}' lives in interface '{}' with no Rust-binding alias",
                                s, en.decl.interface,
                            )
                        })?;
                    return Ok(RetShape::OptionEnum {
                        wit_module,
                        wit_package: en.package.clone(),
                        kebab_name: en.decl.kebab_name.clone(),
                        cases: en.decl.cases.clone(),
                    });
                }
                if let Some(rec) = find_record(records, s, caller_interface) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    return Ok(RetShape::OptionWitValueRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                        helper_snake: rec.helper_snake(),
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
                if let Some(rec) = find_record(records, s, caller_interface) {
                    let type_id_hex: String =
                        rec.type_id.iter().map(|b| format!("{:02x}", b)).collect();
                    return Ok(RetShape::FirstWitValueRecord {
                        kebab_name: rec.kebab_name.clone(),
                        wit_interface: rec.interface.clone(),
                        wit_package: rec.package.clone(),
                        wit_package_version: rec.package_version.clone(),
                        symbolic_name: rec.symbolic_name.clone(),
                        type_id_hex,
                        helper_snake: rec.helper_snake(),
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
            // #677: `list<bool>` — batched predicate result from
            // postgis's `st_*_batch` predicate family
            // (`st_intersects_batch`, `st_contains_batch`, etc.).
            // Surface as JSON array text (e.g. `[true,false,true]`).
            // Symmetric with the param-side `ListListU8` JSON
            // convention; SQL callers consume via `json_each` /
            // SQLite's JSON1 ops.
            WitType::Bool => RetShape::ListBool,
            // #677: `list<list<u8>>` — batched geometry result
            // from postgis's `st_*_batch` geometry family
            // (`st_buffer_batch`, `st_centroid_batch`, etc.). The
            // parser surfaces `list<u8>` as `WitType::ListU8`
            // (not `List(Box<U8>)`), so this case sits OUTSIDE
            // the `WitType::List(inner2)` nested arm below.
            // Encode as JSON-of-int-arrays (e.g.
            // `[[1,2,3],[4,5,6]]`), symmetric with the param-side
            // `ParamShape::ListListU8` convention.
            WitType::ListU8 => RetShape::ListListU8,
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
                // #724: mixed tuple with at least one same-shim record
                // element — mirrors the `ListTupleMixed` param path.
                // `tfloat-batch-from-parquet` return is
                // `list<tuple<string, tfloat-sequence>>`.
                let mixed: Option<Vec<ListTupleElem>> = elems
                    .iter()
                    .map(|e| {
                        if let Some(p) = list_prim_elem(e) {
                            return Some(ListTupleElem::Prim(p));
                        }
                        if let WitType::Unsupported(kebab) = e {
                            if let Some(rec) = find_record(records, kebab, caller_interface) {
                                return Some(ListTupleElem::Record(TupleRecordRef {
                                    kebab_name: rec.kebab_name.clone(),
                                    wit_interface: rec.interface.clone(),
                                    wit_package: rec.package.clone(),
                                    wit_package_version: rec.package_version.clone(),
                                    helper_snake: rec.helper_snake(),
                                }));
                            }
                        }
                        None
                    })
                    .collect();
                if let Some(mixed) = mixed {
                    if !mixed.is_empty()
                        && mixed
                            .iter()
                            .any(|e| matches!(e, ListTupleElem::Record(_)))
                    {
                        return Ok(RetShape::JsonText {
                            kind: JsonRetKind::ListTupleMixed(mixed),
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
        // Gap G3 (#668): bbox3d returns (today: `st-extent-threed`)
        // are rendered as an ISO-WKB `LINESTRING Z` blob whose two
        // vertices are the min and max corners of the bounding box.
        // The diagonal preserves all six coordinates; downstream
        // `st_astext` and other scalar consumers parse it as a
        // standard WKB geometry. Parallels `Bbox => BboxBlob`
        // (which uses `pg_ctor::st_make_envelope`); the 3D form is
        // composed inline because no 3D-envelope constructor exists
        // in the postgis-wasm WIT today.
        WitType::Bbox3d => RetShape::Bbox3dWkbLineZ,
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
            // #707: postgis topology's `topo-geometry` resource has
            // no direct `to-bytes` method. `parse_type` doesn't
            // promote it to a first-class `WitType` variant (the
            // resource only appears as a `create-topo-geom` return
            // today), so it lands here as `Unsupported`. Route to
            // the dedicated RetShape that calls the resource's
            // `geometry()` accessor and serializes the resulting
            // MULTI* geometry via the existing `as-wkb()` path.
            if s == "topo-geometry" {
                return Ok(RetShape::TopoGeometryViaGeom);
            }
            // W3.3 (#543): WIT enums surface here for the same
            // reason params do — parse_type has no Enum variant.
            // Check enums before records (no overlap today; future-proof).
            if let Some(en) = find_enum(enums, s, caller_interface) {
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
            if let Some(rec) = find_record(records, s, caller_interface) {
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
                    helper_snake: rec.helper_snake(),
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

/// #724: mixed-tuple helper-name suffix. Primitives use their
/// `helper_suffix()` (identical to `list_tuple_sig_suffix`);
/// records use `helper_snake` (already disambiguated for
/// same-kebab collisions via `RecordType::helper_snake` per
/// #709/#710). E.g. `[Prim(String), Record(tfloat-sequence)]`
/// → `"string_tfloat_sequence"`.
pub fn list_tuple_mixed_sig_suffix(elements: &[ListTupleElem]) -> String {
    elements
        .iter()
        .map(|e| match e {
            ListTupleElem::Prim(p) => p.helper_suffix().to_string(),
            ListTupleElem::Record(r) => r.helper_snake.clone(),
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// #630: true iff every field of `rec` is a WIT primitive
/// (`bool`, integer width, `f32`/`f64`, `string`) or
/// `option<primitive>`. Drives `JsonRetKind::OptionListPrimRecord`
/// — the variant only fires when `serde_json::to_string(&Vec<R>)`
/// produces a flat array of JSON objects (no nested records, no
/// resources, no lists). `RecordType.fields` carries raw WIT type
/// text (not the parsed `WitType` IR), so the check is structural
/// over the field-text strings, matching the style of
/// `record_registry::field_type_is_direct`.
pub fn record_fields_all_primitive(rec: &crate::record_registry::RecordType) -> bool {
    fn is_prim_text(t: &str) -> bool {
        let t = t.trim();
        matches!(
            t,
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
    fn is_prim_or_option_prim_text(t: &str) -> bool {
        let t = t.trim();
        if is_prim_text(t) {
            return true;
        }
        if let Some(rest) = t.strip_prefix("option<") {
            if let Some(inner) = rest.strip_suffix('>') {
                return is_prim_text(inner.trim());
            }
        }
        false
    }
    rec.fields
        .iter()
        .all(|(_, ft)| is_prim_or_option_prim_text(ft))
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
        WitType::Bbox3d => "bbox3d".into(),
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
    // #673: concatenated-form resource method index (no-hyphen
    // `<resource><method>` key) for SQL aliases like
    // `st_topologynodecount` that omit the separator entirely.
    let method_concat_index = index_resource_methods_concat(&wit_fns);

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
            // 6) #672 resource-family name-matching: relaxes step 5
            //    to any interface in the `<ns>-<resource>-*` family
            //    (e.g. `postgis-topology-edit`, `postgis-topology-
            //    output`, `postgis-topology-query`). Also tries the
            //    swapped form (`validate_topology` for SQL
            //    `topology_validate`) and the joined form
            //    (`create_topology` for SQL `topology_create`).
            // 7) #673 concatenated-form match: catches SQL names
            //    with no separator anywhere (`st_topologynodecount`)
            //    by peeling a resource's no-hyphen kebab off the
            //    front and matching the remainder against either
            //    that resource's methods or sibling-interface
            //    free fns in the resource's `<ns>-<resource>-*`
            //    family.
            let tuple_pick = tuple_pick_override_for(&sc.canonical_name, &wit_fns);
            let matched: Option<&WitFunction> = if let Some((f, _)) = tuple_pick {
                Some(f)
            } else if let Some(f) = override_for(&sc.canonical_name, &wit_fns) {
                Some(f)
            } else if let Some(f) = find_wit_fn(&candidates, &wit_index, &wit_nohyphen) {
                Some(f)
            } else if let Some(f) = find_resource_method(&candidates, &method_index) {
                Some(f)
            } else if let Some(f) = find_same_interface_free_fn(
                &candidates,
                &wit_index,
                &resource_iface_index,
            ) {
                Some(f)
            } else if let Some(f) = find_resource_family_free_fn(
                &candidates,
                &wit_index,
                &resource_iface_index,
            ) {
                Some(f)
            } else if let Some(f) = find_resource_concat_match(
                &candidates,
                &method_concat_index,
                &wit_nohyphen,
                &resource_iface_index,
            ) {
                Some(f)
            } else {
                // #677: dim-variant matching. Routes SQL aliases
                // like `st_3dintersects`, `st_force_2d`,
                // `st_length2d`, `st_perimeter_twod` to the
                // kebab-fix WIT form (`st-<verb>-threed`,
                // `st-force-twod`, etc.).
                find_dim_variant_match(&candidates, &wit_index, &wit_nohyphen)
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
            // #678: scalar-vs-aggregate dual-registration skip.
            //
            // Some shims register the same SQL name in BOTH the
            // `scalars` and `aggregates` tables (postgres exposes
            // aggregates through the same calling convention as
            // scalars, so the interface extractor catches both
            // sides). Today's surface: postgis
            // `st_rast_union_aggregate` and the geometry-side
            // `st_union_aggregate` / `st_polygonize_aggregate`
            // family.
            //
            // The aggregate path wires them correctly via the
            // `build_aggregate_registry` pipeline; the scalar
            // path here finds the same WIT function by name but
            // would otherwise duplicate the entry.
            //
            // #836: Narrow the skip to WIT functions whose FIRST
            // param is `list<borrow<raster>>` (which the scalar
            // arm cannot classify — the `ParamShape::Raster` list
            // form is aggregate-only) OR to the geometry variant
            // ONLY when the SAME SQL name also appears in the
            // aggregate table for this extension. The earlier
            // blanket `list<borrow<geometry>>` skip erroneously
            // dropped legitimate variadic scalars like
            // `st_collect(g1, g2, ...)` — `postgis-accessors::
            // st-collect` is a scalar-shape variadic that the
            // `ParamShape::ListGeom` emit arm handles correctly
            // (both variadic-tail and single-blob-wrapped-as-list
            // flavors), and its SQL name is distinct from the
            // sibling aggregate `st_collect_aggregate`.
            let name_in_aggregates = |name: &str| {
                ext.aggregates.iter().any(|ag| {
                    ag.canonical_name == name
                        || ag.aliases.iter().any(|a| a == name)
                })
            };
            let dual_registered = name_in_aggregates(&sc.canonical_name)
                || sc
                    .aliases
                    .iter()
                    .any(|a| name_in_aggregates(a));
            match f.params.first().map(|p| &p.ty) {
                Some(WitType::ListRasterBorrow) => continue,
                Some(WitType::ListGeomBorrow) if dual_registered => continue,
                _ => {}
            }
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

        // #631: cast-rewrite synthesis pass.
        //
        // Some extension shims emit `cast_rewrites` rows whose
        // `function_name` points at a SQL-callable function that is
        // present in the WIT surface but NOT registered as a row in
        // the interface DB's `scalars` table. The mobilitydb shim
        // does this for `stbox3d_from_text` — the cast rewrite says
        // "CAST(text AS STBOX3D) → stbox3d_from_text(text)" but
        // `register_scalar_function("stbox3d_from_text", ...)` is
        // never called, so the canonical `for sc in &ext.scalars`
        // loop above never sees the name and the dispatch table
        // grows no arm for it.
        //
        // The override mechanism can't rescue this case — overrides
        // run INSIDE the scalars loop, so a missing row stays
        // invisible. We synthesize a `DispatchEntry` by looking up
        // the function in WIT, classifying it, and pushing it onto
        // `entries` directly. The cast_rewrites consumer downstream
        // (sqlink/ducklink/datafission emit) then has a real arm to
        // wire the cast to.
        let known_scalar_names: HashSet<&str> = ext
            .scalars
            .iter()
            .flat_map(|s| {
                std::iter::once(s.canonical_name.as_str())
                    .chain(s.aliases.iter().map(|a| a.as_str()))
            })
            .collect();
        let mut synthesized: HashSet<String> = HashSet::new();
        for cast in &ext.cast_rewrites {
            let fn_name = cast.function_name.as_str();
            if fn_name.is_empty() {
                continue;
            }
            if known_scalar_names.contains(fn_name) {
                continue;
            }
            if !synthesized.insert(fn_name.to_string()) {
                continue;
            }
            // Build a one-element candidate list. Cast rewrites only
            // carry the canonical SQL function name — no aliases —
            // so `find_wit_fn`'s no-hyphen / `st_`-strip fallbacks
            // run against just that name.
            let candidates = vec![fn_name.to_string()];
            let matched = find_wit_fn(&candidates, &wit_index, &wit_nohyphen)
                .or_else(|| find_resource_method(&candidates, &method_index))
                .or_else(|| {
                    find_same_interface_free_fn(
                        &candidates,
                        &wit_index,
                        &resource_iface_index,
                    )
                })
                .or_else(|| {
                    find_resource_family_free_fn(
                        &candidates,
                        &wit_index,
                        &resource_iface_index,
                    )
                })
                .or_else(|| {
                    find_resource_concat_match(
                        &candidates,
                        &method_concat_index,
                        &wit_nohyphen,
                        &resource_iface_index,
                    )
                })
                .or_else(|| {
                    // #677: dim-variant matching for cast-rewrite
                    // synthesis path.
                    find_dim_variant_match(&candidates, &wit_index, &wit_nohyphen)
                });
            let Some(f) = matched else {
                unwired.push(UnwiredScalar {
                    sql_name: fn_name.to_string(),
                    reason: format!(
                        "cast_rewrites references function `{}` (target_type={}, \
                         source_kind={}) but no WIT function matches and the \
                         shim's scalars table has no row for it",
                        fn_name, cast.target_type, cast.source_kind,
                    ),
                });
                continue;
            };
            match classify_shape(f, records, &enums) {
                Ok(shape) => {
                    let fallible = f.ret.fallible;
                    entries.push((
                        DispatchEntry {
                            sql_name: fn_name.to_string(),
                            shape,
                        },
                        fallible,
                    ));
                }
                Err(reason) => unwired.push(UnwiredScalar {
                    sql_name: fn_name.to_string(),
                    reason: format!(
                        "cast_rewrites synthesis: {reason}"
                    ),
                }),
            }
        }
    }
    Ok((entries, unwired))
}

// ──────────────────────────────────────────────────────────────────
// Window function substrate (#616 / PLAN-window-substrate.md).
//
// Window functions are a SEPARATE IR variant from aggregates (DD1):
// the postgis-clustering family is structurally `list<X> -> list<Y>`
// (whole-partition compute), not `list<X> -> single Y` (streaming
// aggregate). Sharing `AggregateShape` would muddy `AccKind` and
// blur the per-target dispatch shapes (sqlite buffer-and-peek,
// duckdb per-frame slice, datafission whole-partition fan-out).
//
// Per DD2, the canonical model is whole-partition compute. The
// sqlite-emit `value()` arm is an adapter (buffer-at-step, compute-
// on-first-value, walk-cursor-on-subsequent-value, drop-at-finalize)
// — frame range doesn't affect the precomputed labels.
//
// Pilot scope: the 4 postgis `st-cluster-*` upstream WIT functions
// (clustering.wit). All share the `list<borrow<geometry>>, ...
// -> list<Y>` shape; `Y` is one of `option<u32>`, `u32`, or
// `geometry`.
// ──────────────────────────────────────────────────────────────────

/// One window-function dispatch arm. `sql_name` is the SQL-side
/// name as the interface DB has it (canonical or alias); the
/// emitter renders one match arm per entry.
pub struct WindowEntry {
    pub sql_name: String,
    pub shape: WindowShape,
}

/// Whole-partition compute shape for one window function.
///
/// Mirrors the field layout of `AggregateShape` for the SHARED
/// machinery (wit_module / wit_package / wit_func), but the
/// per-row-decode + per-row-return semantics are window-specific:
/// the input is the whole partition (rather than streaming rows
/// folded into an accumulator) and the output is `list<Y>` with
/// one entry per input row.
#[derive(Debug, Clone)]
pub struct WindowShape {
    /// Rust binding alias for the owning WIT interface (e.g.
    /// `pg_clust` for `postgis-clustering`). Resolved via
    /// `interface_to_rust_alias`.
    pub wit_module: String,
    /// Owning WIT package (`postgis:wasm`).
    pub wit_package: String,
    /// Snake_case function name on the binding-module side
    /// (e.g. `st_cluster_dbscan`).
    pub wit_func: String,
    /// Extra args after the streaming geometry list (constants,
    /// in declaration order). For `st-cluster-dbscan(geoms, eps,
    /// min-points)`: `[F64, U32]`. For `st-cluster-intersecting(
    /// geoms)`: `[]`.
    pub extra_args: Vec<ParamShape>,
    /// Per-row output shape (the `Y` in `list<Y>`).
    pub returns: WindowReturn,
    /// Upstream-call fallibility: `true` when the WIT function
    /// returns `result<list<Y>, error>` (postgis: always true).
    pub fallible: bool,
    /// OQ3 (locked): order-sensitive flag. Carried in the IR even
    /// though the postgis cluster functions are order-insensitive,
    /// so a future order-sensitive window function (MEOS `tcount`,
    /// etc.) can opt in without IR churn.
    pub order_sensitive: bool,
}

/// Per-row return shape for a window function. One classification
/// per function, applied to every row's emitted value.
///
/// Pilot surface covers the 3 distinct return shapes in the postgis
/// cluster family. Each maps to ONE emit-site recipe per target
/// (sqlite SqlValue::Integer/Null/Blob, duckdb Duckvalue::*, df
/// ScalarValue::*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowReturn {
    /// Per-row `option<u32>` — `st-cluster-dbscan` (cluster id or
    /// NULL for noise points). NULL = SQL NULL.
    OptionU32,
    /// Per-row `u32` — `st-cluster-kmeans` (cluster id 0..k-1).
    U32,
    /// Per-row `geometry` blob — `st-cluster-intersecting`,
    /// `st-cluster-within` (one GeometryCollection per cluster,
    /// emitted as WKB). Sqlite/duckdb wrap as BLOB; datafission
    /// wraps as `ScalarValue::Binary`.
    GeomBlob,
}

/// Classify one upstream WIT function as a window-function shape.
///
/// Accepts the canonical `list<borrow<geometry>>` first param +
/// trailing primitive constants, and one of three return shapes
/// (`result<list<option<u32>>>`, `result<list<u32>>`,
/// `result<list<geometry>>`). Other shapes are rejected with an
/// informative error so the surface stays predictable.
pub fn classify_window_shape(f: &WitFunction) -> Result<WindowShape, String> {
    let alias = wit_parse::interface_to_rust_alias(&f.interface).ok_or_else(|| {
        format!(
            "window interface '{}' has no Rust-binding alias mapping",
            f.interface
        )
    })?;
    let wit_func = wit_parse::kebab_to_snake(&f.kebab_name);

    // First param must be `list<borrow<geometry>>` — the partition
    // input. DD2 locks whole-partition compute; sqlite-emit adapts.
    let Some(first) = f.params.first() else {
        return Err("window function has zero params".into());
    };
    if !matches!(first.ty, WitType::ListGeomBorrow) {
        return Err(format!(
            "window first param must be list<borrow<geometry>>; got {:?}",
            first.ty,
        ));
    }

    // Subsequent params are per-partition constants. The interface
    // DB delivers them as positional SqlValue args at every row;
    // the dispatcher unpacks the first-row args and ignores the
    // rest (they're guaranteed constant across the partition).
    let mut extra = Vec::with_capacity(f.params.len() - 1);
    for (i, p) in f.params.iter().enumerate().skip(1) {
        extra.push(window_extra_arg_shape(&p.ty).map_err(|why| {
            format!("window extra param #{i}: {why}")
        })?);
    }

    // Return must be `list<Y>` for one of the recognised `Y`.
    // Codegen unwraps the `result<...>` engine-side and decodes
    // the inner list.
    let returns = match &f.ret.inner {
        WitType::ListOptionU32 => WindowReturn::OptionU32,
        WitType::ListGeomOwned => WindowReturn::GeomBlob,
        WitType::List(inner) => match inner.as_ref() {
            WitType::U32 => WindowReturn::U32,
            other => {
                return Err(format!(
                    "window return must be list<option<u32>|u32|geometry>; got list<{:?}>",
                    other,
                ));
            }
        },
        other => {
            return Err(format!(
                "window return must be list<option<u32>|u32|geometry>; got {:?}",
                other,
            ));
        }
    };

    Ok(WindowShape {
        wit_module: alias,
        wit_package: f.package.clone(),
        wit_func,
        extra_args: extra,
        returns,
        fallible: f.ret.fallible,
        // Postgis cluster functions are position-stable but not
        // order-sensitive (clustering is order-invariant). Default
        // false until a per-function override surfaces.
        order_sensitive: false,
    })
}

/// Per-row extra-arg classifier for window functions. A reduced
/// subset of `classify_param` — windows take only primitive
/// constants today (`f64`, `u32`, etc.). Reject anything fancier
/// so a future complex-extras window function surfaces as a
/// classifier error rather than silently misdecoding.
fn window_extra_arg_shape(ty: &WitType) -> Result<ParamShape, String> {
    Ok(match ty {
        WitType::S32 => ParamShape::S32,
        WitType::S64 => ParamShape::S64,
        WitType::U32 => ParamShape::U32,
        WitType::U64 => ParamShape::U64,
        WitType::F64 => ParamShape::F64,
        WitType::F32 => ParamShape::F64, // emit_arm uses arg_f64
        WitType::Bool => ParamShape::Bool,
        WitType::String => ParamShape::Text,
        WitType::ListU8 => ParamShape::Blob,
        other => {
            return Err(format!(
                "window extra args must be primitives; got {:?}",
                other,
            ));
        }
    })
}

/// Build the per-extension window-function dispatch registry. For
/// each `BridgePlan::extensions[*].window_functions[*]` row, find
/// the matching upstream WIT function (postgis-clustering interface
/// for the pilot), classify it, and emit per-canonical + per-alias
/// `WindowEntry`s the emit crates can iterate.
///
/// Suffix-strip rules (`win`/`_win`) mirror the postgis convention:
/// `st_clusterintersectingwin` → match `st-cluster-intersecting`
/// against the upstream WIT. 3 of 4 functions ALSO have an
/// aggregate-side entry under the un-suffixed name (DD3 / OQ5);
/// they share upstream entries cleanly because both registrations
/// route through the same upstream function.
pub fn build_window_registry(
    plan: &BridgePlan,
    wit_deps_dir: &Path,
) -> Result<(Vec<WindowEntry>, Vec<UnwiredScalar>)> {
    let wit_fns = wit_parse::parse_dir(wit_deps_dir)?;
    let aliases = collect_package_aliases(wit_deps_dir);
    let wit_fns = resolve_function_aliases(wit_fns, &aliases);

    // Build a snake-name index over every upstream free function
    // (skips resource methods + non-constructors — matches the
    // scalar lookup convention).
    let wit_index = index_wit_fns(&wit_fns);
    let wit_nohyphen = index_wit_fns_nohyphen(&wit_fns);

    let mut entries = Vec::new();
    let mut unwired = Vec::new();
    for ext in &plan.extensions {
        for w in &ext.window_functions {
            let candidates = window_name_candidates(w);
            let matched = find_wit_fn(&candidates, &wit_index, &wit_nohyphen);
            let Some(f) = matched else {
                unwired.push(UnwiredScalar {
                    sql_name: w.canonical_name.clone(),
                    reason: format!(
                        "no WIT function matches any of {:?} for window",
                        candidates
                    ),
                });
                continue;
            };
            match classify_window_shape(f) {
                Ok(shape) => {
                    entries.push(WindowEntry {
                        sql_name: w.canonical_name.clone(),
                        shape: shape.clone(),
                    });
                    for alias in &w.aliases {
                        entries.push(WindowEntry {
                            sql_name: alias.clone(),
                            shape: shape.clone(),
                        });
                    }
                }
                Err(reason) => unwired.push(UnwiredScalar {
                    sql_name: w.canonical_name.clone(),
                    reason,
                }),
            }
        }
    }
    Ok((entries, unwired))
}

/// Generate the candidate name list for a SQL window function.
/// Adds the postgis `_win` / `win` suffix-strip variants AFTER the
/// canonical + aliases so the upstream `st-cluster-intersecting` is
/// reached from SQL `st_clusterintersectingwin`. The base helper
/// reuses the scalar/aggregate candidate sort by Levenshtein.
fn window_name_candidates(w: &WindowFn) -> Vec<String> {
    let mut v = candidates_sorted(&w.canonical_name, &w.aliases);
    // Suffix-strip rules: postgis SQL convention suffixes the WIN
    // form (`st_clusterintersectingwin`); the WIT has the bare
    // form (`st-cluster-intersecting`). Try every existing
    // candidate with `win` / `_win` stripped.
    let mut extra: Vec<String> = Vec::new();
    for cand in &v {
        if let Some(bare) = cand.strip_suffix("_win") {
            extra.push(bare.to_string());
        }
        if let Some(bare) = cand.strip_suffix("win") {
            // Only strip bare `win` if the result is a non-empty,
            // identifier-shaped name (avoid mangling `win`-only or
            // `awin`-style false positives).
            if !bare.is_empty() && bare.ends_with(|c: char| c.is_ascii_alphanumeric()) {
                extra.push(bare.to_string());
            }
        }
    }
    // Dedupe while preserving order (canonical first).
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in v.drain(..).chain(extra.into_iter()) {
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod geog_record_tests {
    //! #753 regression coverage: when the shim's WIT registers a
    //! `record geography` (mobilitydb tgeography surface), classifiers
    //! must route bare / optional geography through the wit-value
    //! record path rather than the postgis resource path. Without
    //! `normalize_ambiguous_records`, `wit_parse::parse_type` eagerly
    //! promotes the kebab to `WitType::Geography` and the classifier
    //! emits `.as_wkb()` + `geog_from_wkb(...)` — both undefined for
    //! the mdb record shape.
    use super::*;
    use crate::record_registry::RecordType;
    use crate::wit_parse::{WitRet, WitType};

    fn make_geog_record() -> RecordType {
        RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "tgeography-ops".to_string(),
            kebab_name: "geography".to_string(),
            fields: vec![
                ("srid".to_string(), "s32".to_string()),
                ("kind".to_string(), "geography-kind".to_string()),
                ("point".to_string(), "option<geog-point>".to_string()),
                ("wkb".to_string(), "list<u8>".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name:
                "mobilitydb:temporal@0.1.0/tgeography-ops/geography".to_string(),
            is_copy: false,
            direct: true,
            kebab_collides_in_pkg: false,
        }
    }

    #[test]
    fn param_geography_with_record_routes_to_witvalue() {
        let records = vec![make_geog_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::Geography { borrowed: false };
        let shape = classify_param(&t, &records, &enums, "tgeography-ops").unwrap();
        assert!(
            matches!(shape, ParamShape::WitValueRecord { .. }),
            "expected WitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn param_geography_without_record_stays_resource() {
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::Geography { borrowed: false };
        let shape = classify_param(&t, &records, &enums, "postgis-scalars").unwrap();
        assert!(matches!(shape, ParamShape::Geog), "expected Geog, got {shape:?}");
    }

    #[test]
    fn return_geography_with_record_routes_to_witvalue() {
        let records = vec![make_geog_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Geography { borrowed: false },
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "tgeography-ops").unwrap();
        assert!(
            matches!(shape, RetShape::WitValueRecord { .. }),
            "expected WitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn return_option_geography_with_record_routes_to_witvalue() {
        let records = vec![make_geog_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Option(Box::new(WitType::Geography { borrowed: false })),
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "tgeography-ops").unwrap();
        assert!(
            matches!(shape, RetShape::OptionWitValueRecord { .. }),
            "expected OptionWitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn return_geography_without_record_stays_resource() {
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Geography { borrowed: false },
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "postgis-scalars").unwrap();
        assert!(matches!(shape, RetShape::GeomBlob), "expected GeomBlob, got {shape:?}");
    }
}

#[cfg(test)]
mod geom_record_tests {
    //! #754 regression coverage: mirror of `geog_record_tests` for the
    //! `geometry` kebab collision — postgis `resource geometry`
    //! (routed via `WitType::Geometry` → `.as_wkb()` /
    //! `geom_from_wkb`) vs. mobilitydb-wasm `record geometry { srid,
    //! kind, point, coords, rings }` in `tgeometry-ops` (mdb #740).
    //!
    //! The mdb record uses STRUCTURAL encoding (point / coords /
    //! rings — no `wkb` field), so the postgis resource-shape
    //! dispatch has neither the method nor the helper needed to
    //! marshal it. Without `normalize_ambiguous_records`,
    //! `wit_parse::parse_type` eagerly promotes the bare kebab to
    //! `WitType::Geometry` and the classifier emits `.as_wkb()` +
    //! `geom_from_wkb(...)`.
    //!
    //! Bonus coverage: `list<geometry>` (parsed as
    //! `WitType::ListGeomOwned`) must also normalize when the record
    //! is present — mobilitydb `tgeometry-values` returns
    //! `list<geometry>` and needs `RetShape::FirstWitValueRecord`
    //! rather than the postgis `FirstGeomBlob`.
    use super::*;
    use crate::record_registry::RecordType;
    use crate::wit_parse::{WitRet, WitType};

    fn make_geom_record() -> RecordType {
        RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "tgeometry-ops".to_string(),
            kebab_name: "geometry".to_string(),
            fields: vec![
                ("srid".to_string(), "s32".to_string()),
                ("kind".to_string(), "geometry-kind".to_string()),
                ("point".to_string(), "option<geom-point>".to_string()),
                ("coords".to_string(), "list<geom-point>".to_string()),
                ("rings".to_string(), "list<list<geom-point>>".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/tgeometry-ops/geometry".to_string(),
            is_copy: false,
            direct: true,
            kebab_collides_in_pkg: false,
        }
    }

    #[test]
    fn param_geometry_with_record_routes_to_witvalue() {
        let records = vec![make_geom_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::Geometry { borrowed: false };
        let shape = classify_param(&t, &records, &enums, "tgeometry-ops").unwrap();
        assert!(
            matches!(shape, ParamShape::WitValueRecord { .. }),
            "expected WitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn param_geometry_without_record_stays_resource() {
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::Geometry { borrowed: false };
        let shape = classify_param(&t, &records, &enums, "postgis-scalars").unwrap();
        assert!(matches!(shape, ParamShape::Geom), "expected Geom, got {shape:?}");
    }

    #[test]
    fn return_geometry_with_record_routes_to_witvalue() {
        let records = vec![make_geom_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Geometry { borrowed: false },
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "tgeometry-ops").unwrap();
        assert!(
            matches!(shape, RetShape::WitValueRecord { .. }),
            "expected WitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn return_option_geometry_with_record_routes_to_witvalue() {
        let records = vec![make_geom_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Option(Box::new(WitType::Geometry { borrowed: false })),
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "tgeometry-ops").unwrap();
        assert!(
            matches!(shape, RetShape::OptionWitValueRecord { .. }),
            "expected OptionWitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn return_list_geometry_with_record_routes_to_witvalue() {
        // #754: `tgeometry-values -> list<geometry>` — parsed as
        // `WitType::ListGeomOwned` by `wit_parse::parse_type`. Must
        // normalize to `List<Unsupported("geometry")>` so
        // `RetShape::FirstWitValueRecord` picks it up.
        let records = vec![make_geom_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::ListGeomOwned,
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "tgeometry-ops").unwrap();
        assert!(
            matches!(shape, RetShape::FirstWitValueRecord { .. }),
            "expected FirstWitValueRecord, got {shape:?}",
        );
    }

    #[test]
    fn return_geometry_without_record_stays_resource() {
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Geometry { borrowed: false },
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "postgis-scalars").unwrap();
        assert!(matches!(shape, RetShape::GeomBlob), "expected GeomBlob, got {shape:?}");
    }

    #[test]
    fn return_list_geometry_without_record_stays_resource() {
        // No record registered → normalization is a no-op and the
        // postgis-style `FirstGeomBlob` shape drives dispatch (as in
        // `postgis-sqlink-bridge`).
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::ListGeomOwned,
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "postgis-scalars").unwrap();
        assert!(
            matches!(shape, RetShape::FirstGeomBlob),
            "expected FirstGeomBlob, got {shape:?}",
        );
    }
}

#[cfg(test)]
mod wit_fn_category_tests {
    //! #768: shape-classifier for `augment_plan_with_upstream_wit_scalars`.
    //!
    //! Covers the ~9 fns flagged by the postgis category audit
    //! (#767) — `_aggregate` / `_agg` / `_win` suffix, primary
    //! aggregate interfaces, and unsuffixed window-shape signatures
    //! (`st-cluster-dbscan`, `st-cluster-kmeans`,
    //! `st-cluster-kmeans-max-radius`) — plus a control set of
    //! ordinary scalars that must NOT be re-routed.
    use super::*;
    use crate::wit_parse::{WitFunction, WitParam, WitRet, WitType};

    fn make_wit_fn(
        interface: &str,
        kebab_name: &str,
        params: Vec<WitParam>,
        ret_inner: WitType,
    ) -> WitFunction {
        WitFunction {
            package: "postgis:wasm".to_string(),
            package_version: "0.1.0".to_string(),
            interface: interface.to_string(),
            kebab_name: kebab_name.to_string(),
            params,
            ret: WitRet {
                inner: ret_inner,
                fallible: false,
                error_ty: None,
            },
            resource: None,
            is_constructor: false,
        }
    }

    fn geom_list_param() -> WitParam {
        WitParam {
            name: "geoms".to_string(),
            ty: WitType::ListGeomBorrow,
        }
    }

    #[test]
    fn aggregate_suffix_routes_to_aggregate() {
        // st-envelope-aggregate, st-coverage-union-aggregate,
        // st-cluster-kmeans-aggregate, st-cluster-dbscan-aggregate,
        // st-union-threed-aggregate, st-rast-union-aggregate — all
        // land here.
        let f = make_wit_fn(
            "postgis-aggregates",
            "st-envelope-aggregate",
            vec![geom_list_param()],
            WitType::Geometry { borrowed: false },
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn agg_suffix_routes_to_aggregate() {
        // mobilitydb's `<name>_agg` duplicates surface with a `-agg`
        // kebab suffix.
        let f = make_wit_fn(
            "temporal-aggregate-ops",
            "tint-min-agg",
            vec![geom_list_param()],
            WitType::S64,
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn primary_aggregate_interface_routes_to_aggregate_without_suffix() {
        // `postgis-aggregates::st-extent-threed` — no `-aggregate`
        // suffix but semantically an aggregate. Interface-name
        // fallback catches it.
        let f = make_wit_fn(
            "postgis-aggregates",
            "st-extent-threed",
            vec![geom_list_param()],
            WitType::Unsupported("bbox3d".to_string()),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn raster_aggregate_interface_routes_to_aggregate() {
        let f = make_wit_fn(
            "postgis-raster-aggregates",
            "st-rast-union-aggregate",
            vec![geom_list_param()],
            WitType::Raster { borrowed: false },
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn win_suffix_routes_to_window() {
        // `st-cluster-intersecting-win`, `st-cluster-within-win` —
        // explicit `-win` suffix.
        let f = make_wit_fn(
            "postgis-clustering",
            "st-cluster-intersecting-win",
            vec![geom_list_param()],
            WitType::List(Box::new(WitType::U32)),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Window);
    }

    #[test]
    fn window_shape_option_u32_routes_to_window_without_suffix() {
        // `st-cluster-dbscan`: list<borrow<geometry>> →
        // list<option<u32>>. No `-win` suffix, but the shape is
        // unambiguously window-shaped.
        let f = make_wit_fn(
            "postgis-clustering",
            "st-cluster-dbscan",
            vec![geom_list_param()],
            WitType::ListOptionU32,
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Window);
    }

    #[test]
    fn window_shape_u32_routes_to_window_without_suffix() {
        // `st-cluster-kmeans`, `st-cluster-kmeans-max-radius`:
        // list<borrow<geometry>> → list<u32>.
        let f = make_wit_fn(
            "postgis-clustering",
            "st-cluster-kmeans",
            vec![geom_list_param()],
            WitType::List(Box::new(WitType::U32)),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Window);
    }

    #[test]
    fn window_shape_kmeans_max_radius_routes_to_window() {
        let f = make_wit_fn(
            "postgis-clustering",
            "st-cluster-kmeans-max-radius",
            vec![geom_list_param()],
            WitType::List(Box::new(WitType::U32)),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Window);
    }

    #[test]
    fn ordinary_scalar_stays_scalar() {
        // `st-area(borrow<geometry>) -> f64` — vanilla scalar.
        let f = make_wit_fn(
            "postgis-measurements",
            "st-area",
            vec![WitParam {
                name: "geom".to_string(),
                ty: WitType::Geometry { borrowed: true },
            }],
            WitType::F64,
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Scalar);
    }

    #[test]
    fn scalar_returning_list_stays_scalar_without_geom_list_input() {
        // A scalar that returns `list<u32>` but does NOT take
        // `list<borrow<geometry>>` as its first arg must not be
        // pulled into the window bucket by the shape check.
        let f = make_wit_fn(
            "postgis-analysis",
            "srid-supported",
            vec![WitParam {
                name: "srid".to_string(),
                ty: WitType::S32,
            }],
            WitType::List(Box::new(WitType::U32)),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Scalar);
    }

    #[test]
    fn window_takes_precedence_over_aggregate_shape() {
        // `-win` suffix wins over `-aggregate` even in a contrived
        // clash — postgis has no such collision today but the
        // ordering keeps future additions predictable.
        let f = make_wit_fn(
            "postgis-clustering",
            "foo-aggregate-win",
            vec![geom_list_param()],
            WitType::List(Box::new(WitType::U32)),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Window);
    }

    #[test]
    fn scalar_geometry_output_from_aggregate_interface_still_aggregate() {
        // `postgis-aggregates::st-union-aggregate` (returns a
        // geometry) — no `-Nd` weirdness, standard aggregate shape.
        let f = make_wit_fn(
            "postgis-aggregates",
            "st-union-aggregate",
            vec![geom_list_param()],
            WitType::Geometry { borrowed: false },
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    // #782: `-aggregate-` and `-agg-` INFIX detection.
    //
    // mobilitydb's temporal-aggregate-ops surface produces names
    // like `int-span-aggregate-union`, `int-spanset-aggregate-union`
    // where `aggregate` sits between two meaningful tokens rather
    // than at the tail. The tail-only checks (#768) mis-routed the
    // ~53 fns from #777 as scalars.

    #[test]
    fn aggregate_infix_int_span_routes_to_aggregate() {
        // `int-span-aggregate-union` — canonical trigger from #777.
        let f = make_wit_fn(
            "temporal-aggregate-ops",
            "int-span-aggregate-union",
            vec![geom_list_param()],
            WitType::Unsupported("int-span".to_string()),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn aggregate_infix_int_spanset_routes_to_aggregate() {
        // `int-spanset-aggregate-union` — same family.
        let f = make_wit_fn(
            "temporal-aggregate-ops",
            "int-spanset-aggregate-union",
            vec![geom_list_param()],
            WitType::Unsupported("int-spanset".to_string()),
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn aggregate_infix_general_routes_to_aggregate() {
        // Interface without `temporal-aggregate-ops` fallback so we
        // isolate the infix detection from the primary-interface
        // detection. `foo-bar-aggregate-baz` still classifies.
        let f = make_wit_fn(
            "some-other-interface",
            "foo-bar-aggregate-baz",
            vec![geom_list_param()],
            WitType::F64,
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    #[test]
    fn agg_infix_routes_to_aggregate() {
        // Short `-agg-` infix form.
        let f = make_wit_fn(
            "some-other-interface",
            "foo-agg-union",
            vec![geom_list_param()],
            WitType::F64,
        );
        assert_eq!(classify_wit_fn_category(&f), WitFnCategory::Aggregate);
    }

    /// #790: end-to-end coverage of `reclassify_plan_categories_against_wit`
    /// on an existing interface-DB row.
    ///
    /// Before #790 the datafission adapter registered
    /// `int_span_aggregate_union` as a scalar because the pre-#782
    /// classifier only checked the `-aggregate` / `-agg` TAIL.
    /// Re-extraction can't fix that (extractor mirrors the adapter);
    /// codegen alone can't either (load_plan reads the DB as-is).
    /// This test builds a plan with the stale scalar row + a
    /// matching WIT file, calls the reclassifier, and asserts the
    /// row moved into `aggregates`.
    #[test]
    fn reclassify_moves_stale_interface_db_scalar_to_aggregate_790() {
        let tmp = std::env::temp_dir().join(format!(
            "datalink-790-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");

        // Mimic the wit_deps layout: one subdir per package.
        let pkg_dir = tmp.join("mobilitydb-temporal");
        std::fs::create_dir_all(&pkg_dir).expect("mkdir pkg");
        std::fs::write(
            pkg_dir.join("aggregates.wit"),
            "package mobilitydb:temporal@0.1.0;\n\
             interface temporal-aggregate-ops {\n  \
                 int-span-aggregate-union: func(xs: list<u32>) -> u32;\n\
             }\n\
             interface temporal-ops {\n  \
                 st-area: func(xs: list<u32>) -> u32;\n\
             }\n",
        )
        .expect("write aggregates.wit");

        // Stale interface-DB shape: aggregate registered as scalar,
        // plus a control scalar that must NOT move.
        let mut plan = shim_bridge_codegen_core::BridgePlan {
            extensions: vec![shim_bridge_codegen_core::Extension {
                name: "mobilitydb".to_string(),
                version: "0.1.0".to_string(),
                api_version: None,
                wasm_path: "unused".to_string(),
                wasm_blake3: "unused".to_string(),
                extracted_at: "unused".to_string(),
                scalars: vec![
                    shim_bridge_codegen_core::ScalarFn {
                        canonical_name: "int_span_aggregate_union".to_string(),
                        aliases: vec!["intspan_union".to_string()],
                        param_signatures: vec![vec!["binary".to_string()]],
                        return_type: "binary".to_string(),
                        is_deterministic: true,
                        propagates_null: true,
                    },
                    shim_bridge_codegen_core::ScalarFn {
                        canonical_name: "st_area".to_string(),
                        aliases: vec![],
                        param_signatures: vec![vec!["binary".to_string()]],
                        return_type: "binary".to_string(),
                        is_deterministic: true,
                        propagates_null: true,
                    },
                ],
                aggregates: vec![],
                table_functions: vec![],
                window_functions: vec![],
                column_types: vec![],
                operators: vec![],
                cast_rewrites: vec![],
                preprocessor_patterns: vec![],
                system_catalog_tables: vec![],
                spatial_indexes: vec![],
            }],
        };

        reclassify_plan_categories_against_wit(&mut plan, &tmp)
            .expect("reclassify");

        // Aggregate moved out of scalars.
        let ext = &plan.extensions[0];
        assert!(
            !ext.scalars.iter().any(|s| s.canonical_name == "int_span_aggregate_union"),
            "int_span_aggregate_union must leave scalars"
        );
        // ... and landed in aggregates, preserving aliases + param sigs.
        let ag = ext
            .aggregates
            .iter()
            .find(|a| a.canonical_name == "int_span_aggregate_union")
            .expect("int_span_aggregate_union in aggregates");
        assert_eq!(ag.aliases, vec!["intspan_union".to_string()]);
        assert_eq!(ag.param_signatures, vec![vec!["binary".to_string()]]);
        // Aggregate defaults mirror the synthesis path.
        assert!(ag.supports_grouped);
        assert!(ag.supports_partial);
        assert!(!ag.is_order_sensitive);
        assert!(!ag.accepts_config);

        // Control scalar stays put — WIT classifies `st-area` as
        // scalar (no aggregate suffix, no window-shape sig).
        assert!(
            ext.scalars.iter().any(|s| s.canonical_name == "st_area"),
            "st_area must remain a scalar; got scalars={:?}",
            ext.scalars.iter().map(|s| &s.canonical_name).collect::<Vec<_>>(),
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #790: augment still sees newly-synthesized fns after the
    /// reclassifier runs. Guards against a regression where the
    /// reclassifier could accidentally consume rows the augment
    /// path was going to synthesize (they arrive in different
    /// buckets and via different codepaths, so the flows are
    /// orthogonal — this pins that guarantee).
    #[test]
    fn reclassify_and_new_synthesis_coexist_790() {
        let tmp = std::env::temp_dir().join(format!(
            "datalink-790-newfn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");

        let pkg_dir = tmp.join("mobilitydb-temporal");
        std::fs::create_dir_all(&pkg_dir).expect("mkdir pkg");
        // WIT declares a fn that has NEVER been registered in the
        // interface DB yet — the augment path should synthesize it,
        // and reclassify should be a no-op wrt this row.
        std::fs::write(
            pkg_dir.join("aggregates.wit"),
            "package mobilitydb:temporal@0.1.0;\n\
             interface temporal-aggregate-ops {\n  \
                 brand-new-aggregate: func(xs: list<u32>) -> u32;\n\
             }\n",
        )
        .expect("write aggregates.wit");

        let mut plan = shim_bridge_codegen_core::BridgePlan {
            extensions: vec![shim_bridge_codegen_core::Extension {
                name: "mobilitydb".to_string(),
                version: "0.1.0".to_string(),
                api_version: None,
                wasm_path: "unused".to_string(),
                wasm_blake3: "unused".to_string(),
                extracted_at: "unused".to_string(),
                // Empty across all buckets — the augment path will
                // populate aggregates.
                scalars: vec![],
                aggregates: vec![],
                table_functions: vec![],
                window_functions: vec![],
                column_types: vec![],
                operators: vec![],
                cast_rewrites: vec![],
                preprocessor_patterns: vec![],
                system_catalog_tables: vec![],
                spatial_indexes: vec![],
            }],
        };

        // Reclassify — no-op because plan has no entries.
        reclassify_plan_categories_against_wit(&mut plan, &tmp)
            .expect("reclassify no-op");
        // Augment — synthesizes `brand_new_aggregate` and routes it
        // into aggregates.
        augment_plan_with_upstream_wit_scalars(&mut plan, &tmp)
            .expect("augment");
        let ext = &plan.extensions[0];
        assert!(
            ext.aggregates.iter().any(|a| a.canonical_name == "brand_new_aggregate"),
            "augment must synthesize brand_new_aggregate as an aggregate; got aggregates={:?}",
            ext.aggregates.iter().map(|a| &a.canonical_name).collect::<Vec<_>>(),
        );
        assert!(
            !ext.scalars.iter().any(|s| s.canonical_name == "brand_new_aggregate"),
            "brand_new_aggregate must NOT land in scalars"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod list_list_record_tests {
    //! #781: `list<list<R>>` param classifier coverage.
    //!
    //! Mobilitydb's spanset-extent-ops interface takes
    //! `list<list<{int,float,date}-span>>` inputs — the outer list
    //! is the batch of spansets and each inner list is one
    //! spanset's spans. Before this variant landed, `classify_param`
    //! fell through to the generic Err on the nested-list-of-record
    //! shape and the four
    //! `<int|float|date|tstz>-spanset-extent` scalars never made it
    //! into the generated bridge's list_functions. The sibling
    //! `spanset-aggregate-union` family is handled on the aggregate
    //! path via #782's `-aggregate-` infix classifier.
    use super::*;
    use crate::record_registry::RecordType;
    use crate::wit_parse::WitType;

    fn make_int_span_record() -> RecordType {
        RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "spans-ops".to_string(),
            kebab_name: "int-span".to_string(),
            fields: vec![
                ("lower".to_string(), "s32".to_string()),
                ("upper".to_string(), "s32".to_string()),
                ("lower-inc".to_string(), "bool".to_string()),
                ("upper-inc".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/spans-ops/int-span".to_string(),
            is_copy: true,
            direct: true,
            kebab_collides_in_pkg: false,
        }
    }

    #[test]
    fn param_list_list_record_routes_to_list_list_record() {
        // `list<list<int-span>>` — the shape from
        // `int-spanset-extent`.
        let records = vec![make_int_span_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::List(Box::new(WitType::List(Box::new(
            WitType::Unsupported("int-span".to_string()),
        ))));
        let shape = classify_param(&t, &records, &enums, "spanset-extent-ops").unwrap();
        match shape {
            ParamShape::ListListRecord {
                kebab_name,
                wit_interface,
                helper_snake,
                ..
            } => {
                assert_eq!(kebab_name, "int-span");
                assert_eq!(wit_interface, "spans-ops");
                assert_eq!(helper_snake, "int_span");
            }
            other => panic!("expected ListListRecord, got {other:?}"),
        }
    }

    #[test]
    fn param_list_list_record_without_matching_record_errors() {
        // No record registered → the classifier can't route the
        // shape and must surface a diagnostic naming the shape so
        // the codegen's per-scalar unwired reason names the gap.
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::List(Box::new(WitType::List(Box::new(
            WitType::Unsupported("mystery-span".to_string()),
        ))));
        let err = classify_param(&t, &records, &enums, "spanset-extent-ops").unwrap_err();
        assert!(
            err.contains("not in dispatcher alphabet"),
            "expected alphabet-diagnostic, got {err}",
        );
    }

    #[test]
    fn param_list_list_prim_still_routes_to_list_list_prim() {
        // Regression: adding `ListListRecord` must not steal the
        // pre-existing `list<list<f64>>` (`ListListPrim`) surface
        // — the prim check runs first inside the nested-list arm.
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::List(Box::new(WitType::List(Box::new(WitType::F64))));
        let shape = classify_param(&t, &records, &enums, "postgis-raster").unwrap();
        assert!(
            matches!(shape, ParamShape::ListListPrim(ListPrimElem::F64)),
            "expected ListListPrim(F64), got {shape:?}",
        );
    }

    #[test]
    fn param_list_list_u8_still_routes_to_list_list_u8() {
        // Regression: `list<list<u8>>` — the postgis batch WKB
        // path — stays on the dedicated ListListU8 arm.
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let t = WitType::List(Box::new(WitType::ListU8));
        let shape = classify_param(&t, &records, &enums, "postgis-batch").unwrap();
        assert!(
            matches!(shape, ParamShape::ListListU8),
            "expected ListListU8, got {shape:?}",
        );
    }
}

#[cfg(test)]
mod option_list_record_tests {
    //! #799 coverage — two shape families identified in the not-wired
    //! mdb-fn audit:
    //!
    //! * `option<list<R>>` return where R has a nested `list<record>`
    //!   field (`t*-append-sequence` — 8 fns): routes through a new
    //!   `JsonRetKind::OptionListRecord` variant. Same serde-based
    //!   emit template as `OptionListPrimRecord`; the split lets
    //!   downstream tooling distinguish "flat rows" from "records
    //!   with nested lists" without a structural walk.
    //!
    //! * `list<list<record>> -> list<record>` aggregate shape
    //!   (`<int|float|date|tstz>-spanset-aggregate-union` — 4 fns):
    //!   routes through a new `AccKind::RecordSetToRecordSet` variant.
    //!   Full emit-path wiring (list-of-list step decode + finalize
    //!   JSON encode) lives in a follow-up; the classifier + force-
    //!   link land here so the four fns register instead of falling
    //!   through the unwired-scalar diagnostic.
    use super::*;
    use crate::record_registry::RecordType;
    use crate::wit_parse::{WitRet, WitType};

    fn make_tint_instant_record() -> RecordType {
        RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "types".to_string(),
            kebab_name: "tint-instant".to_string(),
            fields: vec![
                ("timestamp".to_string(), "s64".to_string()),
                ("value".to_string(), "s64".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/types/tint-instant".to_string(),
            is_copy: true,
            direct: true,
            kebab_collides_in_pkg: false,
        }
    }

    fn make_tint_sequence_record() -> RecordType {
        // Nested `list<tint-instant>` — the field that trips the
        // all-primitive guard on `OptionListPrimRecord`.
        RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "types".to_string(),
            kebab_name: "tint-sequence".to_string(),
            fields: vec![
                ("instants".to_string(), "list<tint-instant>".to_string()),
                ("interpolation".to_string(), "interpolation".to_string()),
                ("lower-inclusive".to_string(), "bool".to_string()),
                ("upper-inclusive".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/types/tint-sequence".to_string(),
            is_copy: false,
            direct: true,
            kebab_collides_in_pkg: false,
        }
    }

    #[test]
    fn return_option_list_record_with_nested_list_record_routes_to_option_list_record() {
        // The `t*-append-sequence` return shape:
        // `option<list<tint-sequence>>`. `tint-sequence` has a
        // `list<tint-instant>` field so `record_fields_all_primitive`
        // returns false; the fallback branch must route through
        // `OptionListRecord`.
        let records = vec![make_tint_instant_record(), make_tint_sequence_record()];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Option(Box::new(WitType::List(Box::new(
                WitType::Unsupported("tint-sequence".to_string()),
            )))),
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "temporal-append-ops").unwrap();
        match shape {
            RetShape::JsonText { kind } => match kind {
                JsonRetKind::OptionListRecord(name) => {
                    assert_eq!(name, "tint-sequence");
                }
                other => panic!("expected OptionListRecord, got {other:?}"),
            },
            other => panic!("expected JsonText/OptionListRecord, got {other:?}"),
        }
    }

    #[test]
    fn return_option_list_prim_record_still_prefers_prim_record_variant() {
        // Regression: an all-primitive record like `int-span` must
        // still route to `OptionListPrimRecord` — the two variants
        // exist so downstream tooling can distinguish "flat rows"
        // from "records with nested lists" without a structural walk.
        let int_span = RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "spans-ops".to_string(),
            kebab_name: "int-span".to_string(),
            fields: vec![
                ("lower".to_string(), "s32".to_string()),
                ("upper".to_string(), "s32".to_string()),
                ("lower-inc".to_string(), "bool".to_string()),
                ("upper-inc".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/spans-ops/int-span".to_string(),
            is_copy: true,
            direct: true,
            kebab_collides_in_pkg: false,
        };
        let records = vec![int_span];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Option(Box::new(WitType::List(Box::new(
                WitType::Unsupported("int-span".to_string()),
            )))),
            fallible: false,
            error_ty: None,
        };
        let shape = classify_return(&r, &records, &enums, "spanset-constructor-ops").unwrap();
        match shape {
            RetShape::JsonText { kind } => match kind {
                JsonRetKind::OptionListPrimRecord(name) => {
                    assert_eq!(name, "int-span");
                }
                other => panic!("expected OptionListPrimRecord, got {other:?}"),
            },
            other => panic!("expected JsonText/OptionListPrimRecord, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_list_list_record_routes_to_record_set_to_record_set() {
        // #799: `int-spanset-aggregate-union` shape:
        //   `func(sets: list<list<int-span>>) -> list<int-span>`.
        // The input is `list<list<record>>` (each row streams a
        // spanset) and the output is `list<record>` (unified
        // spanset). Classifier should build the new
        // `AccKind::RecordSetToRecordSet` variant.
        use crate::wit_parse::{WitFunction, WitParam, WitRet};
        let int_span = RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "spans-ops".to_string(),
            kebab_name: "int-span".to_string(),
            fields: vec![
                ("lower".to_string(), "s32".to_string()),
                ("upper".to_string(), "s32".to_string()),
                ("lower-inc".to_string(), "bool".to_string()),
                ("upper-inc".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/spans-ops/int-span".to_string(),
            is_copy: true,
            direct: true,
            kebab_collides_in_pkg: false,
        };
        let records = vec![int_span];
        let enums: Vec<EnumWithPackage> = vec![];
        let f = WitFunction {
            interface: "span-union-ops".to_string(),
            package: "mobilitydb:temporal".to_string(),
            kebab_name: "int-spanset-aggregate-union".to_string(),
            params: vec![WitParam {
                name: "sets".to_string(),
                ty: WitType::List(Box::new(WitType::List(Box::new(
                    WitType::Unsupported("int-span".to_string()),
                )))),
            }],
            ret: WitRet {
                inner: WitType::List(Box::new(WitType::Unsupported("int-span".to_string()))),
                fallible: false,
                error_ty: None,
            },
            package_version: "0.1.0".to_string(),
            resource: None,
            is_constructor: false,
        };
        let shape = classify_aggregate_shape(&f, &records, &enums).unwrap();
        match shape.accumulator_kind {
            AccKind::RecordSetToRecordSet { input, output } => {
                assert_eq!(input.kebab_name, "int-span");
                assert_eq!(output.kebab_name, "int-span");
            }
            other => panic!("expected RecordSetToRecordSet, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_flat_list_record_still_routes_to_record() {
        // Regression: adding the nested-list arm must not steal the
        // pre-existing flat `list<record>` shape used by mobilitydb's
        // `<int|float|date|tstz>-span-aggregate-union` fns (which
        // share the same span-union-ops interface).
        use crate::wit_parse::{WitFunction, WitParam, WitRet};
        let int_span = RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "spans-ops".to_string(),
            kebab_name: "int-span".to_string(),
            fields: vec![
                ("lower".to_string(), "s32".to_string()),
                ("upper".to_string(), "s32".to_string()),
                ("lower-inc".to_string(), "bool".to_string()),
                ("upper-inc".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/spans-ops/int-span".to_string(),
            is_copy: true,
            direct: true,
            kebab_collides_in_pkg: false,
        };
        let records = vec![int_span];
        let enums: Vec<EnumWithPackage> = vec![];
        let f = WitFunction {
            interface: "span-union-ops".to_string(),
            package: "mobilitydb:temporal".to_string(),
            kebab_name: "int-span-aggregate-union".to_string(),
            params: vec![WitParam {
                name: "spans".to_string(),
                ty: WitType::List(Box::new(WitType::Unsupported("int-span".to_string()))),
            }],
            ret: WitRet {
                inner: WitType::List(Box::new(WitType::Unsupported("int-span".to_string()))),
                fallible: false,
                error_ty: None,
            },
            package_version: "0.1.0".to_string(),
            resource: None,
            is_constructor: false,
        };
        let shape = classify_aggregate_shape(&f, &records, &enums).unwrap();
        assert!(
            matches!(shape.accumulator_kind, AccKind::Record { .. }),
            "expected flat list<record> to stay on AccKind::Record, got {:?}",
            shape.accumulator_kind,
        );
    }

    #[test]
    fn aggregate_list_record_to_list_prim_string_routes_to_record_to_list_prim() {
        // #830: `tjsonb-sequences-agg-collect-keys` shape:
        //   `func(seqs: list<tjsonb-sequence>) -> list<string>`.
        // Input `list<record>` (AccKind::Record family) + output
        // `list<primitive>` — classifier should build the new
        // `AccKind::RecordToListPrim` variant with a String element.
        use crate::wit_parse::{WitFunction, WitParam, WitRet};
        let tjsonb_sequence = RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "types".to_string(),
            kebab_name: "tjsonb-sequence".to_string(),
            fields: vec![
                ("instants".to_string(), "list<tjsonb-instant>".to_string()),
                ("interpolation".to_string(), "interpolation".to_string()),
                ("lower-inclusive".to_string(), "bool".to_string()),
                ("upper-inclusive".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/types/tjsonb-sequence".to_string(),
            is_copy: false,
            direct: true,
            kebab_collides_in_pkg: false,
        };
        let records = vec![tjsonb_sequence];
        let enums: Vec<EnumWithPackage> = vec![];
        let f = WitFunction {
            interface: "temporal-aggregate-ops".to_string(),
            package: "mobilitydb:temporal".to_string(),
            kebab_name: "tjsonb-sequences-agg-collect-keys".to_string(),
            params: vec![WitParam {
                name: "seqs".to_string(),
                ty: WitType::List(Box::new(WitType::Unsupported(
                    "tjsonb-sequence".to_string(),
                ))),
            }],
            ret: WitRet {
                inner: WitType::List(Box::new(WitType::String)),
                fallible: false,
                error_ty: None,
            },
            package_version: "0.1.0".to_string(),
            resource: None,
            is_constructor: false,
        };
        let shape = classify_aggregate_shape(&f, &records, &enums).unwrap();
        match shape.accumulator_kind {
            AccKind::RecordToListPrim { input, output } => {
                assert_eq!(input.kebab_name, "tjsonb-sequence");
                assert_eq!(output, ListPrimElem::String);
            }
            other => panic!("expected RecordToListPrim, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_list_record_to_list_u32_routes_to_record_to_list_prim() {
        // #830 generalisation: `list<record> -> list<u32>` (or any
        // primitive width) should follow the same path. Belt-and-
        // suspenders: ensures the classifier isn't over-fitted to
        // the `list<string>` case that motivated the fix.
        use crate::wit_parse::{WitFunction, WitParam, WitRet};
        let tint_sequence = RecordType {
            package: "mobilitydb:temporal".to_string(),
            package_version: "0.1.0".to_string(),
            interface: "types".to_string(),
            kebab_name: "tint-sequence".to_string(),
            fields: vec![
                ("instants".to_string(), "list<tint-instant>".to_string()),
                ("interpolation".to_string(), "interpolation".to_string()),
                ("lower-inclusive".to_string(), "bool".to_string()),
                ("upper-inclusive".to_string(), "bool".to_string()),
            ],
            type_id: [0u8; 32],
            symbolic_name: "mobilitydb:temporal@0.1.0/types/tint-sequence".to_string(),
            is_copy: false,
            direct: true,
            kebab_collides_in_pkg: false,
        };
        let records = vec![tint_sequence];
        let enums: Vec<EnumWithPackage> = vec![];
        let f = WitFunction {
            interface: "temporal-aggregate-ops".to_string(),
            package: "mobilitydb:temporal".to_string(),
            kebab_name: "tint-sequences-agg-something".to_string(),
            params: vec![WitParam {
                name: "seqs".to_string(),
                ty: WitType::List(Box::new(WitType::Unsupported(
                    "tint-sequence".to_string(),
                ))),
            }],
            ret: WitRet {
                inner: WitType::List(Box::new(WitType::U32)),
                fallible: false,
                error_ty: None,
            },
            package_version: "0.1.0".to_string(),
            resource: None,
            is_constructor: false,
        };
        let shape = classify_aggregate_shape(&f, &records, &enums).unwrap();
        match shape.accumulator_kind {
            AccKind::RecordToListPrim { input, output } => {
                assert_eq!(input.kebab_name, "tint-sequence");
                assert_eq!(output, ListPrimElem::U32);
            }
            other => panic!("expected RecordToListPrim, got {other:?}"),
        }
    }

    #[test]
    fn return_option_list_record_missing_registry_errors_cleanly() {
        // If the record isn't in the registry, the classifier still
        // reports a named diagnostic naming the missing record so
        // the codegen's unwired-scalar reason surfaces the gap.
        let records: Vec<RecordType> = vec![];
        let enums: Vec<EnumWithPackage> = vec![];
        let r = WitRet {
            inner: WitType::Option(Box::new(WitType::List(Box::new(
                WitType::Unsupported("mystery-sequence".to_string()),
            )))),
            fallible: false,
            error_ty: None,
        };
        let err = classify_return(&r, &records, &enums, "temporal-append-ops").unwrap_err();
        assert!(
            err.contains("not in dispatcher alphabet"),
            "expected alphabet-diagnostic, got {err}",
        );
    }
}
