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
    CapabilityKind, FnDecl, NeutralType, NeutralValue, NullHandling,
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
mod shim_duckdb;
mod shim_duckdb_agg;
mod shim_embed;
mod shim_sqlite;
