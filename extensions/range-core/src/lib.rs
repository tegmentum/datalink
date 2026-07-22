//! Neutral core for the `range` extension — a minimal TABLE-valued
//! function exercising `datalink_extcore::declare!`'s TABLE production
//! (T3 in the table-capability pull-up).
//!
//! The one declared function, `range_from_to(int64, int64) -> (v: int64)`,
//! mirrors the shape of a genome-format / BLAST TVF (input row → many
//! output rows, one column) but uses the smallest possible arg + column
//! surface so its behavior is trivially verifiable end-to-end under
//! real DuckLink:
//!
//! ```sql
//! LOAD 'ducklink.duckdb_extension';
//! FROM ducklink_load('range.wasm');
//! SELECT * FROM range_from_to(0, 5);
//! -- expected: 5 rows (0, 1, 2, 3, 4)
//! ```
//!
//! The core is `no_std`; the per-DB shim (`duckdb_shim!` in
//! `range-component`) marshals `NeutralValue` <-> `Duckvalue`.

#![no_std]

extern crate alloc;

use alloc::vec;
use datalink_extcore::NeutralValue;

datalink_extcore::declare! {
    core = Core;
    extension = "range";
    version = env!("CARGO_PKG_VERSION");

    table range_from_to(int64, int64) -> (v: int64) [deterministic] = |args| {
        // args: &[NeutralValue] — the single input row (start, end).
        // return: Result<Vec<Vec<NeutralValue>>, String>
        //   — outer vec = rows, inner vec = row's columns in declared order.
        use datalink_extcore::ArgExt as _;
        let start = args.arg_int(0, "range_from_to")?;
        let end = args.arg_int(1, "range_from_to")?;
        if end < start {
            return Ok(vec![]);
        }
        Ok((start..end).map(|v| vec![NeutralValue::Int64(v)]).collect())
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::{CapabilityKind, ExtCore};

    #[test]
    fn declares_range_from_to_as_table() {
        assert_eq!(Core::DECLS.len(), 1);
        let d = &Core::DECLS[0];
        assert_eq!(d.name, "range_from_to");
        assert_eq!(d.kind, CapabilityKind::Table);
        assert_eq!(d.columns.len(), 1);
        assert_eq!(d.columns[0].name, "v");
    }

    #[test]
    fn dispatch_table_produces_the_range() {
        let rows = Core::dispatch_table(0, &[NeutralValue::Int64(0), NeutralValue::Int64(5)])
            .expect("range dispatch");
        assert_eq!(rows.len(), 5);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.as_slice(), &[NeutralValue::Int64(i as i64)]);
        }
    }

    #[test]
    fn empty_range_when_end_le_start() {
        let rows = Core::dispatch_table(0, &[NeutralValue::Int64(5), NeutralValue::Int64(5)])
            .expect("empty range dispatch");
        assert!(rows.is_empty());
    }
}
