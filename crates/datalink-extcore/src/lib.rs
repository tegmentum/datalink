//! Codegen for the extension pull-up.
//!
//! Write an extension ONCE — its DB-agnostic logic plus a capability
//! [`declare!`] table — and generate BOTH the ducklink
//! (`duckdb:extension`) and sqlink (`sqlite:extension`) shims, instead
//! of hand-maintaining the same ~8-line algorithm in three places with
//! three different registration ABIs and value-marshalling conventions.
//!
//! # The split
//!
//! Per the design pass (#137), the function LOGIC is byte-for-byte
//! identical across the two ports; ~80-85% of each extension's source is
//! pure GLUE that varies only in names/arg-ret-types — exactly what a
//! DECLARATION names. So:
//!
//!   * the CORE crate ([`crate::declare!`]) owns the logic + the
//!     capability table ([`FnDecl`] slice) + a neutral
//!     [`ExtCore::dispatch`];
//!   * the SHIM is fully derived by [`duckdb_shim!`] / [`sqlite_shim!`] /
//!     [`embed_shim!`] from that declaration + the value model.
//!
//! # Per-repo WIT parameterization
//!
//! The two repos are on different, FROZEN contracts (ducklink
//! `duckdb:extension@2.2.0` + wit-bindgen 0.41; sqlink
//! `sqlite:extension@1.0.0` + wit-bindgen 0.44), and each consuming
//! crate runs its own `wit_bindgen::generate!`. The shim macros are
//! therefore parameterized by the binding PATHS the consuming crate
//! exposes — they never hardcode one repo's package/version. Generated
//! code names only those paths plus this crate's neutral types.
//!
//! # The frozen-type-set rule
//!
//! The marshalling here targets the FROZEN v1 value arms only. Anything
//! outside the closed set rides [`NeutralValue::Complex`] →
//! `complex(type-expr, json)` / `wit-value`. The codegen NEVER emits a
//! new `duckvalue`/`logicaltype`/`sql-value` arm.

#![no_std]

extern crate alloc;

pub use datalink_valuemodel::{
    CapabilityKind, FnDecl, NeutralColVec, NeutralColumn, NeutralType, NeutralValue,
    NullHandling,
};

use alloc::string::String;
use alloc::vec::Vec;

/// Shared per-chunk scalar dispatch loop used by the generated `duckdb`
/// shims ([`duckdb_shim!`](crate::duckdb_shim) /
/// [`duckdb_agg_shim!`](crate::duckdb_agg_shim)).
///
/// # Batching wins
///
/// This is the hot path a scalar query pays for every DataChunk. The
/// caller resolves the handle ONCE (`idx`) and reads the function's
/// NULL-handling ONCE (`propagate`) before the loop, instead of the old
/// shape that delegated to `call_scalar` PER ROW — each delegation took a
/// `Mutex` lock + a `HashMap` lookup to re-resolve the same handle. The
/// loop also reuses a single neutral scratch `Vec` across all rows
/// (cleared + refilled), so the per-row inner allocation that the old
/// `args.iter().map(to_neutral).collect()` made every row vanishes after
/// the first.
///
/// Generic over the host value type `V` (a contract's `duckvalue`) so the
/// same loop serves any `duckdb:extension` minor without naming a WIT
/// type. Semantically IDENTICAL to calling `call_scalar` per row: row `i`
/// maps to `out[i]`, and a [`NullHandling::Propagate`] function yields
/// NULL for any row with a NULL argument WITHOUT invoking the core.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn scalar_batch<V, E>(
    idx: usize,
    propagate: bool,
    rows: Vec<Vec<V>>,
    to_neutral: impl Fn(&V) -> NeutralValue,
    from_neutral: impl Fn(NeutralValue) -> V,
    null_value: impl Fn() -> V,
    dispatch: impl Fn(usize, &[NeutralValue]) -> Result<NeutralValue, String>,
    map_err: impl Fn(String) -> E,
) -> Result<Vec<V>, E> {
    let mut out = Vec::with_capacity(rows.len());
    // One reused scratch buffer for the whole chunk (was one alloc/row).
    let mut neutral: Vec<NeutralValue> = Vec::new();
    for args in rows.into_iter() {
        neutral.clear();
        neutral.extend(args.iter().map(&to_neutral));
        if propagate && neutral.iter().any(NeutralValue::is_null) {
            out.push(null_value());
            continue;
        }
        let res = dispatch(idx, &neutral).map_err(&map_err)?;
        out.push(from_neutral(res));
    }
    Ok(out)
}

/// Columnar scalar dispatch — the guest side of the proposed major-4
/// `call-scalar-batch-col` ABI. The host hands one [`NeutralColVec`] per
/// argument (each a contiguous typed buffer + a packed validity bitmap),
/// the generated columnar shim having lifted them via a bulk `memcpy`
/// across the canonical ABI instead of the row-major
/// `list<list<duckvalue>>` per-cell tagged-variant serialization. This is
/// where the ~82-110x boundary win is realized (see the
/// `columnar-abi-prototype` bench).
///
/// The kernel here still bridges to the per-row `dispatch` so EVERY
/// existing core works under the columnar ABI with zero per-core changes:
/// only the boundary representation changed, not the core's neutral logic.
/// A core that later ships a true vectorized kernel can supersede this
/// bridge, but it is never required. Semantics are identical to
/// [`scalar_batch`]: row `i` ⇒ `out[i]`, and a [`NullHandling::Propagate`]
/// function yields NULL (validity bit cleared) for any row with a NULL
/// argument without invoking the core.
///
/// `ret` is the declared return [`NeutralType`]; the output column is a
/// matching typed buffer. The reused `scratch` neutral row buffer means a
/// chunk allocates only the output column + (lazily) one validity byte
/// vector, not 2048 inner vectors.
pub fn scalar_batch_col(
    idx: usize,
    propagate: bool,
    ret: &NeutralType,
    args: &[NeutralColVec],
    dispatch: impl Fn(usize, &[NeutralValue]) -> Result<NeutralValue, String>,
) -> Result<NeutralColVec, String> {
    let rows = args.first().map(|c| c.rows).unwrap_or(0);
    let mut out = OutColumn::with_capacity(ret, rows);
    let mut scratch: Vec<NeutralValue> = Vec::with_capacity(args.len());
    let mut validity = ValidityBuilder::new(rows);
    for r in 0..rows {
        scratch.clear();
        let mut any_null = false;
        for col in args {
            let v = col.value_at(r);
            any_null |= v.is_null();
            scratch.push(v);
        }
        if propagate && any_null {
            out.push(NeutralValue::Null);
            validity.set_null(r);
            continue;
        }
        let res = dispatch(idx, &scratch)?;
        if res.is_null() {
            validity.set_null(r);
        }
        out.push(res);
    }
    Ok(NeutralColVec {
        data: out.finish(),
        validity: validity.finish(),
        rows,
    })
}

/// Accumulates a typed output column for [`scalar_batch_col`], pushing one
/// [`NeutralValue`] per row into the buffer matching the declared return
/// type. A value whose type does not match the declared return is coerced
/// to the column's NULL slot (the validity bitmap records the NULL); this
/// mirrors the row-major path, where a type mismatch is the core's bug, not
/// a panic.
enum OutColumn {
    Boolean(Vec<bool>),
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Text(Vec<String>),
    Blob(Vec<Vec<u8>>),
    Complex { type_expr: String, json: Vec<String> },
}

impl OutColumn {
    fn with_capacity(ret: &NeutralType, rows: usize) -> Self {
        match ret {
            NeutralType::Boolean => OutColumn::Boolean(Vec::with_capacity(rows)),
            NeutralType::Int64 => OutColumn::Int64(Vec::with_capacity(rows)),
            NeutralType::Float64 => OutColumn::Float64(Vec::with_capacity(rows)),
            NeutralType::Text => OutColumn::Text(Vec::with_capacity(rows)),
            NeutralType::Blob => OutColumn::Blob(Vec::with_capacity(rows)),
            NeutralType::Complex(te) => OutColumn::Complex {
                type_expr: te.clone(),
                json: Vec::with_capacity(rows),
            },
        }
    }

    #[inline]
    fn push(&mut self, v: NeutralValue) {
        match (self, v) {
            (OutColumn::Boolean(b), NeutralValue::Boolean(x)) => b.push(x),
            (OutColumn::Boolean(b), _) => b.push(false),
            (OutColumn::Int64(b), NeutralValue::Int64(x)) => b.push(x),
            (OutColumn::Int64(b), _) => b.push(0),
            (OutColumn::Float64(b), NeutralValue::Float64(x)) => b.push(x),
            (OutColumn::Float64(b), _) => b.push(0.0),
            (OutColumn::Text(b), NeutralValue::Text(x)) => b.push(x),
            (OutColumn::Text(b), _) => b.push(String::new()),
            (OutColumn::Blob(b), NeutralValue::Blob(x)) => b.push(x),
            (OutColumn::Blob(b), _) => b.push(Vec::new()),
            (OutColumn::Complex { json, .. }, NeutralValue::Complex { json: j, .. }) => {
                json.push(j)
            }
            (OutColumn::Complex { json, .. }, _) => json.push(String::new()),
        }
    }

    fn finish(self) -> NeutralColumn {
        match self {
            OutColumn::Boolean(b) => NeutralColumn::Boolean(b),
            OutColumn::Int64(b) => NeutralColumn::Int64(b),
            OutColumn::Float64(b) => NeutralColumn::Float64(b),
            OutColumn::Text(b) => NeutralColumn::Text(b),
            OutColumn::Blob(b) => NeutralColumn::Blob(b),
            OutColumn::Complex { type_expr, json } => {
                NeutralColumn::Complex { type_expr, json }
            }
        }
    }
}

/// Lazily-allocated packed validity bitmap. Stays empty (zero allocation)
/// until the first NULL is recorded, matching the all-valid fast path of
/// DuckDB's null validity pointer and [`NeutralColVec::all_valid`].
struct ValidityBuilder {
    rows: usize,
    bits: Vec<u8>,
}

impl ValidityBuilder {
    fn new(rows: usize) -> Self {
        ValidityBuilder { rows, bits: Vec::new() }
    }

    #[inline]
    fn set_null(&mut self, row: usize) {
        if self.bits.is_empty() {
            // Allocate all-valid (all bits set), then clear this row.
            self.bits = alloc::vec![0xFF; (self.rows + 7) / 8];
        }
        let byte = row >> 3;
        if byte < self.bits.len() {
            self.bits[byte] &= !(1u8 << (row & 7));
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bits
    }
}

/// The contract an extension core implements. `declare!` generates this
/// impl; the shim macros consume it. Cores are `no_std`-friendly and
/// never reference any host WIT type.
pub trait ExtCore {
    /// The extension name (the manifest/loadresult `name`).
    const NAME: &'static str;
    /// The extension version (typically `env!("CARGO_PKG_VERSION")`).
    const VERSION: &'static str;
    /// The capability table — the single source of truth both shims read.
    const DECLS: &'static [FnDecl];

    /// Invoke the function at index `idx` in [`Self::DECLS`] with neutral
    /// arguments, producing a neutral result (or an error message). The
    /// shim has already applied [`NullHandling`] before calling this.
    fn dispatch(idx: usize, args: &[NeutralValue]) -> Result<NeutralValue, String>;

    /// Fold an aggregate group. `idx` is the [`Self::DECLS`] index of an
    /// [`Aggregate`](datalink_valuemodel::CapabilityKind::Aggregate)
    /// function; `rows` is the whole buffered group (each inner slice is
    /// one row's neutral args). Returns the finalized neutral value.
    ///
    /// Because the `duckdb:extension` host buffers a group and makes a
    /// single `call_aggregate`, the entire `init` → `step*` → `finalize`
    /// fold runs here in one call: the neutral state stays a native Rust
    /// value and never marshals across the WIT boundary. NULL/empty-group
    /// handling lives in the core's `step`/`finalize` (the shim passes
    /// every buffered row through verbatim, matching the hand-written
    /// aggregates' per-row skip).
    ///
    /// The default returns an error so scalar-only cores need not
    /// implement it; [`declare!`](crate::declare) overrides it for any
    /// core that declares an `aggregate`.
    fn dispatch_aggregate(
        idx: usize,
        rows: &[&[NeutralValue]],
    ) -> Result<NeutralValue, String> {
        let _ = rows;
        Err(alloc::format!(
            "{}: function index {} is not an aggregate",
            Self::NAME,
            idx
        ))
    }
}

/// Ergonomic neutral-argument extraction for use inside core logic
/// bodies. These mirror the hand-written `arg_text`/`arg_int`/`arg_blob`
/// helpers that every extension copy-pasted, lifted to the neutral
/// value so a core writes them once.
pub trait ArgExt {
    /// Extract a text argument at `i`. Accepts [`NeutralValue::Text`];
    /// decodes a [`NeutralValue::Blob`] as UTF-8 (matching the existing
    /// hand-written `arg_text` BLOB fallthrough).
    fn arg_text(&self, i: usize, fname: &str) -> Result<String, String>;
    /// Extract an integer argument at `i` ([`NeutralValue::Int64`]).
    fn arg_int(&self, i: usize, fname: &str) -> Result<i64, String>;
    /// Extract a blob argument at `i`. Accepts [`NeutralValue::Blob`];
    /// uses a [`NeutralValue::Text`]'s UTF-8 bytes (matching the existing
    /// hand-written `arg_blob` TEXT fallthrough).
    fn arg_blob(&self, i: usize, fname: &str) -> Result<alloc::vec::Vec<u8>, String>;
    /// Extract a float argument at `i`. Accepts [`NeutralValue::Float64`]
    /// and widens a [`NeutralValue::Int64`] to `f64` (matching the
    /// existing hand-written `f64_arg` INTEGER fallthrough used by
    /// geometric extensions like `geohash`).
    fn arg_float(&self, i: usize, fname: &str) -> Result<f64, String>;
}

impl ArgExt for [NeutralValue] {
    fn arg_text(&self, i: usize, fname: &str) -> Result<String, String> {
        match self.get(i) {
            Some(NeutralValue::Text(s)) => Ok(s.clone()),
            Some(NeutralValue::Blob(b)) => String::from_utf8(b.clone())
                .map_err(|_| alloc::format!("{fname}: BLOB is not valid UTF-8")),
            // Matches ducklink aba's `arg_text`: NULL coerces to "".
            Some(NeutralValue::Null) => Ok(String::new()),
            _ => Err(alloc::format!("{fname}: expected TEXT arg at position {i}")),
        }
    }

    fn arg_int(&self, i: usize, fname: &str) -> Result<i64, String> {
        match self.get(i) {
            Some(NeutralValue::Int64(n)) => Ok(*n),
            _ => Err(alloc::format!("{fname}: expected INTEGER arg at position {i}")),
        }
    }

    fn arg_blob(&self, i: usize, fname: &str) -> Result<alloc::vec::Vec<u8>, String> {
        match self.get(i) {
            Some(NeutralValue::Blob(b)) => Ok(b.clone()),
            Some(NeutralValue::Text(s)) => Ok(s.as_bytes().to_vec()),
            _ => Err(alloc::format!("{fname}: expected BLOB arg at position {i}")),
        }
    }

    fn arg_float(&self, i: usize, fname: &str) -> Result<f64, String> {
        match self.get(i) {
            Some(NeutralValue::Float64(f)) => Ok(*f),
            Some(NeutralValue::Int64(n)) => Ok(*n as f64),
            _ => Err(alloc::format!("{fname}: expected FLOAT arg at position {i}")),
        }
    }
}

mod declare;
mod shim_columnar_bridge;
mod shim_duckdb;
mod shim_duckdb_agg;
mod shim_embed;
mod shim_sqlite;
mod shim_sqlite_agg;

#[cfg(test)]
mod columnar_tests {
    use super::*;
    use alloc::vec;

    // +1 i64 scalar; CALLED null-handling (inspects the NULL itself).
    fn plus_one(_idx: usize, args: &[NeutralValue]) -> Result<NeutralValue, String> {
        match args.first() {
            Some(NeutralValue::Int64(n)) => Ok(NeutralValue::Int64(n + 1)),
            _ => Ok(NeutralValue::Null),
        }
    }

    #[test]
    fn columnar_matches_rowmajor_all_valid() {
        let n = 2048usize;
        let data: Vec<i64> = (0..n as i64).collect();
        let col = NeutralColVec::all_valid(NeutralColumn::Int64(data.clone()));
        let out = scalar_batch_col(0, false, &NeutralType::Int64, &[col], plus_one).unwrap();
        assert_eq!(out.rows, n);
        assert!(out.validity.is_empty(), "all-valid stays zero-alloc");
        match out.data {
            NeutralColumn::Int64(v) => {
                for (i, x) in v.iter().enumerate() {
                    assert_eq!(*x, i as i64 + 1);
                }
            }
            _ => panic!("wrong out type"),
        }
    }

    #[test]
    fn columnar_propagate_sets_validity_for_null_rows() {
        // rows 3 and 7 NULL; propagate => result NULL at those rows.
        let n = 10usize;
        let data: Vec<i64> = (0..n as i64).collect();
        let mut validity = vec![0xFFu8; (n + 7) / 8];
        for &null_row in &[3usize, 7] {
            validity[null_row >> 3] &= !(1u8 << (null_row & 7));
        }
        let col = NeutralColVec {
            data: NeutralColumn::Int64(data),
            validity,
            rows: n,
        };
        let out = scalar_batch_col(0, true, &NeutralType::Int64, &[col], plus_one).unwrap();
        for r in 0..n {
            let valid = out.is_valid(r);
            if r == 3 || r == 7 {
                assert!(!valid, "row {r} should be NULL");
            } else {
                assert!(valid, "row {r} should be valid");
                assert_eq!(out.value_at(r), NeutralValue::Int64(r as i64 + 1));
            }
        }
    }

    #[test]
    fn columnar_empty_chunk() {
        let col = NeutralColVec::all_valid(NeutralColumn::Int64(Vec::new()));
        let out = scalar_batch_col(0, false, &NeutralType::Int64, &[col], plus_one).unwrap();
        assert_eq!(out.rows, 0);
        assert_eq!(out.data, NeutralColumn::Int64(Vec::new()));
    }
}
