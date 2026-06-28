//! `sqlite:extension`-shape virtual-table schema emission.
//!
//! Phase 1 of #566 carves the vtab CREATE TABLE schema generator
//! and the visible-column count helper out of `emit_lib.rs` into
//! their own module. Multi-column row materialisation (#532) lives
//! in `emit_lib::emit_row_materialiser` for now — it's tightly
//! coupled to the SqlValue dispatch loop and moves once that loop
//! settles into its post-migration shape.

use crate::dispatch;

/// Build the `CREATE TABLE x(...)` schema literal for one UDTF.
/// Output columns (visible) come first, then HIDDEN columns (one
/// per WIT param). Quoted identifiers use double quotes so kebab
/// names like `min-x` survive the SQL parse. Task #531.
pub fn build_vtab_schema(sql_name: &str, shape: &dispatch::UdtfShape) -> String {
    let mut cols: Vec<String> = Vec::new();
    let mut visible_names: std::collections::BTreeSet<String> = Default::default();
    // Visible columns from the WIT return-row shape.
    match &shape.output_row {
        dispatch::UdtfOutputRow::SingleGeom => {
            let name = dispatch::udtf_single_geom_column_name(sql_name);
            cols.push(format!("\"{name}\" BLOB"));
            visible_names.insert(name.to_string());
        }
        dispatch::UdtfOutputRow::SinglePrimitive { affinity } => {
            cols.push(format!("\"value\" {}", affinity.as_str()));
            visible_names.insert("value".to_string());
        }
        dispatch::UdtfOutputRow::Record { fields } => {
            for f in fields {
                cols.push(format!(
                    "\"{name}\" {aff}",
                    name = f.name,
                    aff = f.affinity.as_str(),
                ));
                visible_names.insert(f.name.clone());
            }
        }
        dispatch::UdtfOutputRow::Unwired { .. } => {
            // Fall back to a single BLOB so the vtab still loads.
            cols.push("\"value\" BLOB".to_string());
            visible_names.insert("value".to_string());
        }
    }
    // HIDDEN columns for the function's params, in WIT order.
    // Collision resolution: if a WIT param name matches a visible
    // column, fall back to `_arg<i>` so SQLite doesn't reject the
    // schema for duplicate column names. We also dedupe across the
    // HIDDEN cols themselves (the WIT signature theoretically
    // permits two params with the same name).
    let mut used: std::collections::BTreeSet<String> = visible_names.clone();
    for (i, name) in shape.param_names.iter().enumerate() {
        let chosen = if used.contains(name) {
            format!("_arg{i}")
        } else {
            name.clone()
        };
        used.insert(chosen.clone());
        cols.push(format!("\"{chosen}\" HIDDEN"));
    }
    format!("CREATE TABLE x({})", cols.join(", "))
}

/// Number of visible (non-HIDDEN) columns the vtab declares for
/// this UDTF. Used by the column arm to decide whether an
/// xColumn(col) lookup hits the output projection or a HIDDEN
/// input. Task #531.
pub fn visible_column_count(row: &dispatch::UdtfOutputRow) -> usize {
    match row {
        dispatch::UdtfOutputRow::SingleGeom => 1,
        dispatch::UdtfOutputRow::SinglePrimitive { .. } => 1,
        dispatch::UdtfOutputRow::Record { fields } => fields.len(),
        dispatch::UdtfOutputRow::Unwired { .. } => 1,
    }
}
