//! Hand-curated SQL-name → WIT-function overrides.
//!
//! Two tables today:
//!
//! - `operator_function_overrides`: SQL names whose canonical
//!   resolver misses (the standard snake/kebab path fails because
//!   the SQL name and the WIT name share no stem). Routes to a
//!   specific `(interface, kebab-name)` pair.
//! - `tuple_pick_overrides`: SQL-side accessor names that route
//!   to a tuple-returning WIT function and surface a single
//!   element of the tuple as the SQL return.
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
pub fn operator_function_overrides() -> &'static [(&'static str, &'static str, &'static str)] {
    // (sql_name, wit_interface, wit_kebab_name)
    &[
        ("st_bboxintersects",   "postgis-operators", "op-bbox-intersects-twod"),
        ("st_bboxintersectsnd", "postgis-operators", "op-bbox-intersects-nd"),
        ("st_knndistance",      "postgis-operators", "op-knn-distance"),
        ("st_bboxdistance",     "postgis-operators", "op-bbox-distance"),
        ("st_geomequal",        "postgis-operators", "op-equals-spatially"),
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
