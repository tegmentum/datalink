//! Hand-curated SQL-name → WIT-function overrides.
//!
//! Three tables today:
//!
//! - `operator_function_overrides`: SQL names whose canonical
//!   resolver misses (the standard snake/kebab path fails because
//!   the SQL name and the WIT name share no stem). Routes to a
//!   specific `(interface, kebab-name)` pair.
//! - `tuple_pick_overrides`: SQL-side accessor names that route
//!   to a tuple-returning WIT function and surface a single
//!   element of the tuple as the SQL return.
//! - `aggregate_function_overrides`: SQL aggregate names whose
//!   canonical name + alias list shares no stem with the upstream
//!   WIT aggregate function. Routes to a specific
//!   `(interface, kebab-name)` pair. Sibling to
//!   `operator_function_overrides` but consulted on the aggregate
//!   path inside `build_aggregate_registry`. Round (#608).
//!
//! `RetShape` rewrite for tuple-pick lives in
//! `emit_sqlite::dispatch` because it consumes the dispatch-IR
//! return shape; once the IR types migrate to `core/interface_db`
//! the rewrite helper can move here too.

use crate::wit_parse::WitFunction;

/// Operator-function name overrides — SQL-side names that the
/// codegen explicitly routes to a non-default WIT function
/// (typically a `postgis-operators::op-*` entry). The standard
/// snake-to-kebab resolver misses these because the names don't
/// share a stem.
///
/// #639 batch 2/3: also routes mobilitydb EWKT-text scalars whose
/// SQL surface drops the `_instant_` infix that the upstream WIT
/// kebab carries. The interface DB advertises both `tX_from_ewkt`
/// (bare) and `tX_instant_from_ewkt` for the instant variant; the
/// explicit-instant SQL name resolves directly via the snake
/// index, but the bare name shares no stem with `tX-instant-from-
/// ewkt` and needs the hand-curated route. Sequence variants
/// (`tX_sequence_from_ewkt`) resolve directly when the shim
/// advertises them — no override needed there.
pub fn operator_function_overrides() -> &'static [(&'static str, &'static str, &'static str)] {
    // (sql_name, wit_interface, wit_kebab_name)
    &[
        ("st_bboxintersects",   "postgis-operators", "op-bbox-intersects-twod"),
        ("st_bboxintersectsnd", "postgis-operators", "op-bbox-intersects-nd"),
        ("st_knndistance",      "postgis-operators", "op-knn-distance"),
        ("st_bboxdistance",     "postgis-operators", "op-bbox-distance"),
        ("st_geomequal",        "postgis-operators", "op-equals-spatially"),
        // #639 batch 2: EWKT text parsers — bare SQL name routes to
        // the `-instant-` WIT variant (matching the convention also
        // used by the existing `tgeompoint3d_from_ewkt` etc. shim
        // entries). The explicit `tX_instant_from_ewkt` SQL aliases
        // resolve via the snake index without an override.
        ("tbool_from_ewkt",       "tbool-ops",       "tbool-instant-from-ewkt"),
        ("tint_from_ewkt",        "tint-ops",        "tint-instant-from-ewkt"),
        ("tfloat_from_ewkt",      "tfloat-ops",      "tfloat-instant-from-ewkt"),
        ("ttext_from_ewkt",       "ttext-ops",       "ttext-instant-from-ewkt"),
        ("tgeompoint_from_ewkt",  "tgeompoint-ops",  "tgeompoint-instant-from-ewkt"),
        // #639 batch 3 — tgeogpoint variants. The SQL surface for
        // these doesn't ship in the current interface DB; the
        // override is preregistered so it fires automatically once
        // the shim advertises the corresponding SQL functions.
        ("tgeogpoint_from_ewkt",  "tgeogpoint-ops",  "tgeogpoint-instant-from-ewkt"),
    ]
}

/// Look up an operator-function override. Returns the matching
/// `WitFunction` (by walking `wit_fns`) if the SQL name has a
/// hand-curated route.
pub fn override_for<'a>(
    sql_name: &str,
    wit_fns: &'a [WitFunction],
) -> Option<&'a WitFunction> {
    let entry = operator_function_overrides()
        .iter()
        .find(|(name, _, _)| *name == sql_name)?;
    wit_fns
        .iter()
        .find(|f| f.interface == entry.1 && f.kebab_name == entry.2)
}

/// #564: tuple-pick overrides — SQL-side accessor names that route
/// to a tuple-returning WIT function and surface a single element.
///
/// Each entry pairs a SQL name with the underlying (interface,
/// kebab-name) plus the zero-based tuple index to project. The
/// underlying function's params are reused verbatim; the dispatch
/// arm calls it and emits `Ok(SqlValue::<variant>(result.<idx>))`.
///
/// Today's surface is the two postgis raster pixel-coord accessors
/// that share `st-world-to-raster-coord -> tuple<s32, s32>` — the
/// upstream `tuple<s32, s32>` is already wired for the bare
/// `st_worldtorastercoord` SQL function (W3.5 #551, as JSON text);
/// these entries expose the per-element scalar projections.
///
/// Sibling to `operator_function_overrides()` — same hand-curated
/// route mechanism, different payload shape (carries the tuple
/// index in addition to the interface + kebab name).
pub fn tuple_pick_overrides() -> &'static [(
    &'static str,
    &'static str,
    &'static str,
    usize,
)] {
    // (sql_name, wit_interface, wit_kebab_name, tuple_index)
    &[
        (
            "st_worldtorastercoordcol",
            "postgis-raster-pixels",
            "st-world-to-raster-coord",
            0,
        ),
        (
            "st_worldtorastercoordrow",
            "postgis-raster-pixels",
            "st-world-to-raster-coord",
            1,
        ),
    ]
}

/// Look up a tuple-pick override. Returns the underlying
/// `WitFunction` (by walking `wit_fns`) plus the tuple index to
/// project. `None` if the SQL name has no tuple-pick entry or the
/// referenced underlying function isn't present in the parsed WIT.
pub fn tuple_pick_override_for<'a>(
    sql_name: &str,
    wit_fns: &'a [WitFunction],
) -> Option<(&'a WitFunction, usize)> {
    let entry = tuple_pick_overrides()
        .iter()
        .find(|(name, _, _, _)| *name == sql_name)?;
    let f = wit_fns
        .iter()
        .find(|f| f.interface == entry.1 && f.kebab_name == entry.2)?;
    Some((f, entry.3))
}

/// Round (#608): aggregate-function name overrides — SQL aggregate
/// names whose canonical name + alias list share no stem with the
/// upstream WIT aggregate function.
///
/// Sibling to `operator_function_overrides`: same hand-curated
/// `(sql_name, wit_interface, wit_kebab_name)` shape, but consulted
/// in `build_aggregate_registry` BEFORE the regular candidate-list
/// name match. Each routed entry feeds the standard
/// `classify_aggregate_shape` pipeline downstream, so the override
/// only fixes the WIT-function lookup — return-shape classification
/// (e.g. `bbox3d` → `Bbox3dWkbLineZ`) is still computed from the WIT
/// signature.
///
/// Today's surface:
/// - `st_3dextent` → `postgis-aggregates::st-extent-threed`. The
///   WIT identifier was renamed from `st-3d-extent` to
///   `st-extent-threed` (per `aggregates.wit` comment) to satisfy
///   wit-bindgen 0.37+'s "each hyphen segment must start with a
///   letter" rule. The interface DB's `st_3d_extent` alias bridges
///   the prefix style but not the trailing rename, so name-match
///   misses; this entry routes it directly.
/// - #615 mobilitydb name-match misses: the SQL aggregate names
///   diverge from the upstream `temporal-aggregate-ops` kebab names
///   by more than the `_agg` suffix-strip path can bridge. Each
///   entry routes a single SQL name at an upstream
///   `tX-temporal-Y` function whose signature
///   `func(sequences: list<X-sequence>) -> option<X-sequence>` is
///   the same-record case downstream classification already handles.
///     - `tfloat_total` → `tfloat-temporal-sum`: SQL stem `total`
///       has no shared root with WIT stem `sum`, so neither direct
///       match nor `_agg`-strip helps.
///     - `tint_min_agg` / `tint_max_agg` / `tint_sum_agg`: the
///       upstream WIT names are `tint-temporal-{min,max,sum}`; the
///       SQL names lack the `temporal-` infix so name-match misses
///       even after `_agg` strip (`tint_min` vs `tint-temporal-min`).
///     - `tgeompoint_centroid_agg` → `tgeompoint-temporal-centroid`:
///       same `temporal-` infix mismatch (`tgeompoint_centroid` vs
///       `tgeompoint-temporal-centroid`).
/// - #660 batch — five postgis aggregates whose SQL canonical names
///   (`st_clusterintersecting`, `st_makeline`, `st_union`,
///   `st_polygonize`, `st_rast_union`) share no stem with the upstream
///   WIT kebabs (long-form + `-aggregate` suffix). The
///   `_aggregate`-suffix strip on the candidate side can't bridge a
///   bare SQL stem to an `_aggregate`-suffixed WIT name, so route each
///   directly. Four go to `postgis-aggregates`; the raster variant
///   goes to `postgis-raster-aggregates`.
pub fn aggregate_function_overrides() -> &'static [(&'static str, &'static str, &'static str)] {
    // (sql_name, wit_interface, wit_kebab_name)
    &[
        ("st_3dextent", "postgis-aggregates", "st-extent-threed"),
        // #615 mobilitydb temporal-aggregate-ops name-match misses.
        ("tfloat_total", "temporal-aggregate-ops", "tfloat-temporal-sum"),
        ("tint_min_agg", "temporal-aggregate-ops", "tint-temporal-min"),
        ("tint_max_agg", "temporal-aggregate-ops", "tint-temporal-max"),
        ("tint_sum_agg", "temporal-aggregate-ops", "tint-temporal-sum"),
        (
            "tgeompoint_centroid_agg",
            "temporal-aggregate-ops",
            "tgeompoint-temporal-centroid",
        ),
        // #636 batch 1 — upstream mobilitydb-wasm 8cdc881 added 4
        // whole-aggregate (set-shaped) WIT entries under
        // `_aggregate` suffix. The interface DB still spells them
        // with `_agg`; the existing `_agg`-strip fallback yields
        // the bare scalar stem (`tint_count`) which doesn't exist
        // as a WIT entry. Route directly to the new entries.
        ("tint_count_agg", "temporal-aggregate-ops", "tint-count-aggregate"),
        (
            "tint_min_value_agg",
            "temporal-aggregate-ops",
            "tint-min-value-aggregate",
        ),
        (
            "tint_max_value_agg",
            "temporal-aggregate-ops",
            "tint-max-value-aggregate",
        ),
        (
            "ttext_concat_agg",
            "temporal-aggregate-ops",
            "ttext-concat-aggregate",
        ),
        // #639 batch 2 — upstream mobilitydb-wasm ada1857 added two
        // more whole-aggregate (set-shaped) WIT entries under the
        // `_aggregate` suffix. Same name-match miss pattern as the
        // #636 entries above: the interface DB spells them with
        // `_agg`, suffix-strip fallback yields the bare stem
        // (`tfloat_variance` / `tint_range`) which doesn't exist
        // as a WIT entry. Route directly.
        (
            "tfloat_variance_agg",
            "temporal-aggregate-ops",
            "tfloat-variance-aggregate",
        ),
        (
            "tint_range_agg",
            "temporal-aggregate-ops",
            "tint-range-aggregate",
        ),
        // #660 batch — postgis aggregate name-match misses. The SQL
        // canonical names (no underscores, e.g. `st_clusterintersecting`
        // / `st_makeline`) and the upstream WIT kebabs (long-form +
        // `-aggregate` suffix, e.g. `st-cluster-intersecting-aggregate`)
        // share no stem after snake/kebab normalisation, so the
        // candidate-list lookup misses. The `_aggregate`-suffix-strip
        // fallback strips FROM the candidate, not from the WIT name, so
        // it can't bridge a bare SQL stem to an `_aggregate`-suffixed
        // WIT name either. Route each directly. The classifier still
        // picks up the return shape (`list<geometry>` →
        // `RetShape::FirstGeomBlob` for cluster-intersecting; bare
        // `geometry` → `RetShape::GeomBlob` for the rest; bare `raster`
        // → `RetShape::RasterBlob` for the rast variant).
        (
            "st_clusterintersecting",
            "postgis-aggregates",
            "st-cluster-intersecting-aggregate",
        ),
        (
            "st_makeline",
            "postgis-aggregates",
            "st-make-line-aggregate",
        ),
        ("st_union", "postgis-aggregates", "st-union-aggregate"),
        (
            "st_polygonize",
            "postgis-aggregates",
            "st-polygonize-aggregate",
        ),
        (
            "st_rast_union",
            "postgis-raster-aggregates",
            "st-rast-union-aggregate",
        ),
        // #663 batch — 11 additional postgis aggregate name-match gaps
        // surfaced by post-#660 regen. Each canonical name listed below
        // appears in the interface DB's `aggregates` table as its own row
        // (not folded into the canonical names already routed above), so
        // name-match lookup misses for the same reason as #660: the SQL
        // stems either include the `aggregate`/`agg` suffix outright or
        // diverge from the WIT kebab by more than the `_aggregate`/`_agg`
        // suffix-strip can bridge. Route each at the same WIT entry the
        // #660 canonical already wires. `st_3d_extent` is the underscore
        // sibling of the existing `st_3dextent` route. `st_clusterwithin`
        // is the bare-stem aggregate variant of `st-cluster-within`; the
        // upstream `postgis-aggregates::st-cluster-within-aggregate`
        // signature (`list<borrow<geometry>>, f64 -> list<geometry>`)
        // matches the SQL aggregate shape and classifies the same way as
        // the #660 `st_clusterintersecting` route.
        ("st_3d_extent", "postgis-aggregates", "st-extent-threed"),
        (
            "st_clusterwithin",
            "postgis-aggregates",
            "st-cluster-within-aggregate",
        ),
        (
            "st_clusterintersectingaggregate",
            "postgis-aggregates",
            "st-cluster-intersecting-aggregate",
        ),
        (
            "st_clusterwithinaggregate",
            "postgis-aggregates",
            "st-cluster-within-aggregate",
        ),
        ("st_makelineagg", "postgis-aggregates", "st-make-line-aggregate"),
        (
            "st_makelineaggregate",
            "postgis-aggregates",
            "st-make-line-aggregate",
        ),
        (
            "st_polygonizeagg",
            "postgis-aggregates",
            "st-polygonize-aggregate",
        ),
        (
            "st_polygonizeaggregate",
            "postgis-aggregates",
            "st-polygonize-aggregate",
        ),
        (
            "st_raster_union",
            "postgis-raster-aggregates",
            "st-rast-union-aggregate",
        ),
        ("st_unionagg", "postgis-aggregates", "st-union-aggregate"),
        ("st_unionaggregate", "postgis-aggregates", "st-union-aggregate"),
    ]
}

/// Round (#608): look up an aggregate-function override. Returns the
/// matching `WitFunction` (by walking `wit_fns`) if the SQL name has
/// a hand-curated route. Sibling to `override_for`.
pub fn aggregate_override_for<'a>(
    sql_name: &str,
    wit_fns: &'a [WitFunction],
) -> Option<&'a WitFunction> {
    let entry = aggregate_function_overrides()
        .iter()
        .find(|(name, _, _)| *name == sql_name)?;
    wit_fns
        .iter()
        .find(|f| f.interface == entry.1 && f.kebab_name == entry.2)
}
