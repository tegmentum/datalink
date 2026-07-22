//! Database-agnostic neutral value model.
//!
//! An extension core (e.g. `aba-core`, `baseN-core`) declares its
//! capability surface ONCE in terms of these neutral types, and the
//! per-DB shims (`duckdb_shim!` / `sqlite_shim!` / `embed_shim!` in
//! `datalink-extcore`) are CODEGEN'd from that declaration. The core
//! never names a `Duckvalue` or a `SqlValue`; it speaks [`NeutralType`]
//! and [`NeutralValue`], and the generated shim marshals to/from the
//! host's own WIT value variant.
//!
//! # The closed set + the escape hatch
//!
//! The two host contracts are intentionally NOT symmetric:
//!
//!   * ducklink `duckdb:extension@2.2.0` — a 21-arm `duckvalue`
//!     (`boolean … uuid`) plus one `complex(complexvalue)` arm carrying
//!     `(type-expr, json)`.
//!   * sqlink `sqlite:extension@1.0.0` — a 5-arm `sql-value`
//!     (`null integer real text blob`) plus one `wit-value` arm carrying
//!     a CBOR payload.
//!
//! Rather than enumerate the 21-vs-5 difference, this model targets the
//! INTERSECTION that every certified provider can represent natively —
//! the [`NeutralType`] closed set — and routes anything else through a
//! single [`NeutralType::Complex`] / [`NeutralValue::Complex`] arm. That
//! arm is the neutral spelling of DuckDB `complex(type-expr, json)` and
//! SQLite `wit-value`: each DB owns one escape arm, so the neutral layer
//! stays small and FUTURE types ride the escape hatch with no contract
//! bump and no new value arm. This is the FROZEN-v1-type-set rule from
//! the design: never emit a new duckvalue/logicaltype arm — route it
//! through `Complex`.
//!
//! Boolean is in the closed set as a LOGICAL type even though SQLite has
//! no native boolean: the codegen knows the per-DB convention (DuckDB
//! `Boolean(bool)` vs SQLite `Integer(0|1)`), so the core can declare a
//! `Boolean` return and both shims do the right thing.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The closed set of neutral SQL types an extension core may name in a
/// capability declaration. Every arm here is representable natively by
/// both host contracts (modulo the documented boolean convention);
/// anything outside the set rides [`NeutralType::Complex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NeutralType {
    /// Logical boolean. DuckDB: `Boolean`. SQLite: `Integer` (0/1).
    Boolean,
    /// 64-bit signed integer. DuckDB: `Int64`. SQLite: `Integer`.
    Int64,
    /// IEEE-754 double. DuckDB: `Float64`. SQLite: `Real`.
    Float64,
    /// UTF-8 text. DuckDB: `Text`. SQLite: `Text`.
    Text,
    /// Binary blob. DuckDB: `Blob`. SQLite: `Blob`.
    Blob,
    /// The escape hatch. The carried string is a DuckDB type-expression
    /// (e.g. `"INTEGER[]"`, `"STRUCT(a INTEGER, b VARCHAR)"`). DuckDB
    /// maps it to `Logicaltype::Complex(type-expr)`; SQLite maps it to a
    /// `wit-value` whose symbolic name is the type-expression. New
    /// composite types ride this arm — never add a closed-set arm.
    Complex(String),
}

/// A neutral SQL value, mirroring [`NeutralType`] one-to-one plus an
/// explicit [`NeutralValue::Null`]. The generated shim converts these
/// to/from the host value variant; the core only ever sees these.
#[derive(Clone, Debug, PartialEq)]
pub enum NeutralValue {
    /// SQL NULL.
    Null,
    /// Logical boolean.
    Boolean(bool),
    /// 64-bit signed integer.
    Int64(i64),
    /// IEEE-754 double.
    Float64(f64),
    /// UTF-8 text.
    Text(String),
    /// Binary blob.
    Blob(Vec<u8>),
    /// The escape-hatch value: `(type-expr, json)`. Mirrors DuckDB
    /// `complexvalue { type-expr, json }`; on SQLite the `json` is the
    /// payload and `type-expr` the symbolic name.
    Complex { type_expr: String, json: String },
}

impl NeutralValue {
    /// The [`NeutralType`] this value carries, or `None` for [`Null`]
    /// (NULL has no intrinsic neutral type — it inhabits any).
    ///
    /// [`Null`]: NeutralValue::Null
    pub fn neutral_type(&self) -> Option<NeutralType> {
        match self {
            NeutralValue::Null => None,
            NeutralValue::Boolean(_) => Some(NeutralType::Boolean),
            NeutralValue::Int64(_) => Some(NeutralType::Int64),
            NeutralValue::Float64(_) => Some(NeutralType::Float64),
            NeutralValue::Text(_) => Some(NeutralType::Text),
            NeutralValue::Blob(_) => Some(NeutralType::Blob),
            NeutralValue::Complex { type_expr, .. } => {
                Some(NeutralType::Complex(type_expr.clone()))
            }
        }
    }

    /// True if this is [`NeutralValue::Null`]. The codegen uses this for
    /// the per-DB NULL-propagation convention.
    pub fn is_null(&self) -> bool {
        matches!(self, NeutralValue::Null)
    }
}

/// A typed, contiguous neutral COLUMN — the columnar counterpart of a
/// per-cell [`NeutralValue`] list. This is the neutral spelling of the
/// proposed major-4 `duckdb:extension` columnar ABI (`column` variant):
/// one buffer per physical type, so the WIT canonical-ABI crossing is a
/// bulk `memcpy` for fixed-width arms instead of a per-cell tagged-variant
/// serialization. Variable-width arms (`Text`/`Blob`) stay element-wise
/// (unavoidable), and anything outside the closed set rides
/// [`NeutralColumn::Complex`] exactly as [`NeutralValue::Complex`] does.
///
/// Measured win of the columnar boundary over the row-major
/// `list<list<duckvalue>>` boundary: ~82-110x on a 1M-row i64 scalar (the
/// canonical-ABI marshalling drops from ~73 ns/row to ~0.9 ns/row); see
/// the `columnar-abi-prototype` bench. NULLs are carried out-of-band in a
/// packed validity bitmap so the data buffer stays a flat typed array.
#[derive(Clone, Debug, PartialEq)]
pub enum NeutralColumn {
    /// Logical boolean column.
    Boolean(Vec<bool>),
    /// 64-bit signed integer column (also carries the physical-int aliases
    /// the host widens — date/time/timestamp ride the host `column` arms).
    Int64(Vec<i64>),
    /// IEEE-754 double column.
    Float64(Vec<f64>),
    /// UTF-8 text column (element-wise; var-width).
    Text(Vec<String>),
    /// Binary blob column (element-wise; var-width).
    Blob(Vec<Vec<u8>>),
    /// Escape-hatch column: one `(type-expr, json)` per row. `type_expr`
    /// is shared across the column; `json` is per-row.
    Complex {
        /// The shared DuckDB type-expression for every element.
        type_expr: String,
        /// The per-row JSON-rendered values.
        json: Vec<String>,
    },
}

impl NeutralColumn {
    /// Number of rows in the column.
    pub fn len(&self) -> usize {
        match self {
            NeutralColumn::Boolean(v) => v.len(),
            NeutralColumn::Int64(v) => v.len(),
            NeutralColumn::Float64(v) => v.len(),
            NeutralColumn::Text(v) => v.len(),
            NeutralColumn::Blob(v) => v.len(),
            NeutralColumn::Complex { json, .. } => json.len(),
        }
    }

    /// True if the column has no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The [`NeutralType`] every element of this column carries.
    pub fn neutral_type(&self) -> NeutralType {
        match self {
            NeutralColumn::Boolean(_) => NeutralType::Boolean,
            NeutralColumn::Int64(_) => NeutralType::Int64,
            NeutralColumn::Float64(_) => NeutralType::Float64,
            NeutralColumn::Text(_) => NeutralType::Text,
            NeutralColumn::Blob(_) => NeutralType::Blob,
            NeutralColumn::Complex { type_expr, .. } => {
                NeutralType::Complex(type_expr.clone())
            }
        }
    }

    /// Read row `i` as a [`NeutralValue`] (the bridge to per-row core logic
    /// for cores that have not yet been ported to a columnar kernel).
    /// `valid` is this row's validity bit (false ⇒ [`NeutralValue::Null`]).
    pub fn value_at(&self, i: usize, valid: bool) -> NeutralValue {
        if !valid {
            return NeutralValue::Null;
        }
        match self {
            NeutralColumn::Boolean(v) => NeutralValue::Boolean(v[i]),
            NeutralColumn::Int64(v) => NeutralValue::Int64(v[i]),
            NeutralColumn::Float64(v) => NeutralValue::Float64(v[i]),
            NeutralColumn::Text(v) => NeutralValue::Text(v[i].clone()),
            NeutralColumn::Blob(v) => NeutralValue::Blob(v[i].clone()),
            NeutralColumn::Complex { type_expr, json } => NeutralValue::Complex {
                type_expr: type_expr.clone(),
                json: json[i].clone(),
            },
        }
    }
}

/// A neutral column plus its packed validity bitmap and row count — the
/// neutral counterpart of the proposed major-4 `colvec` record. `validity`
/// is a packed little-endian bitmap (bit `i` set ⇒ row `i` valid); an
/// EMPTY bitmap means "all rows valid", mirroring DuckDB's null validity
/// pointer (the common, fast case allocates nothing).
#[derive(Clone, Debug, PartialEq)]
pub struct NeutralColVec {
    /// The typed data buffer.
    pub data: NeutralColumn,
    /// Packed validity bits; empty ⇒ all valid.
    pub validity: Vec<u8>,
    /// The logical row count.
    pub rows: usize,
}

impl NeutralColVec {
    /// Construct an all-valid column (empty validity bitmap).
    pub fn all_valid(data: NeutralColumn) -> Self {
        let rows = data.len();
        NeutralColVec {
            data,
            validity: Vec::new(),
            rows,
        }
    }

    /// True if row `i` is valid (non-NULL). An empty bitmap ⇒ all valid.
    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        if self.validity.is_empty() {
            return true;
        }
        let byte = i >> 3;
        byte >= self.validity.len() || (self.validity[byte] >> (i & 7)) & 1 != 0
    }

    /// Read row `i` as a [`NeutralValue`], honoring validity.
    pub fn value_at(&self, i: usize) -> NeutralValue {
        self.data.value_at(i, self.is_valid(i))
    }
}

/// How a scalar handles a NULL argument. This is part of the capability
/// declaration so the generated shim encodes the contract uniformly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NullHandling {
    /// If ANY argument is NULL, the result is NULL without invoking the
    /// core logic (the SQL "strict"/RETURNS NULL ON NULL INPUT default).
    /// This matches ducklink's `aba_validate(NULL) -> NULL`.
    Propagate,
    /// The core logic is invoked even when an argument is NULL (it
    /// inspects the [`NeutralValue::Null`] itself).
    Called,
}

/// The kind of capability a declared function exposes. The pull-up
/// targets the codegen-able tiers; richer kinds stay hand-written per
/// the capability gradient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityKind {
    /// A scalar function: N neutral args in, one neutral value out.
    Scalar,
    /// An aggregate function: folds a group of rows (each row N neutral
    /// args) into one neutral value. The core declares the fold as a
    /// neutral `state` type plus `init` / `step` / `finalize`; the
    /// generated shim composes them per the host's aggregate ABI. On the
    /// `duckdb:extension` contract the host buffers a group's rows and
    /// makes ONE `call_aggregate`, so the fold runs entirely in-guest and
    /// the state never crosses the WIT boundary (no state marshalling).
    Aggregate,
    /// A table-valued function: N neutral args in, a set of rows out.
    /// Each output row carries a value per column declared in
    /// [`FnDecl::columns`]. On `duckdb:extension` the host calls
    /// `call_table` and receives a `Resultset` (`list<list<duckvalue>>`).
    ///
    /// Design choice — kept `FnDecl` as a flat struct (rather than
    /// splitting into `enum FnDecl { Scalar/Aggregate/Table(...) }`)
    /// because `columns` is the ONLY new field a table declaration adds
    /// on top of the existing scalar/aggregate fields (args/ret/kind);
    /// the empty-slice default keeps scalar/aggregate declarations
    /// literal-compatible without an enum-visitor overhaul of every
    /// existing shim (all six shims iterate `.kind`, `.args`, `.ret`).
    /// The only downstream callers of `FnDecl { ... }` literals live
    /// inside `datalink-extcore/src/declare.rs` itself, so the field
    /// addition is a strictly-local change.
    Table,
}

/// One column of a table-valued function's output schema. A `Table`
/// declaration carries `&'static [ColDecl]` in [`FnDecl::columns`]; the
/// `duckdb:extension` shim maps each entry to a
/// `runtime::Columndef { name, logical }` when it registers the TVF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColDecl {
    /// The column's SQL-visible name (e.g. `"query_key"`).
    pub name: &'static str,
    /// The column's neutral type. Follows the FROZEN closed set; ride
    /// [`NeutralType::Complex`] for anything outside it.
    pub ntype: NeutralType,
}

/// One declared function in an extension's capability table. A
/// [`slice`](core::slice) of these IS the extension's neutral surface;
/// both per-DB shims are derived from it, so surface drift between the
/// two databases becomes structurally impossible.
///
/// `columns` and `replacement_scan_extensions` are meaningful only for
/// [`CapabilityKind::Table`] entries and default to empty for
/// scalar/aggregate. The struct stays flat (not an enum) because every
/// consuming shim already iterates `.kind`/`.args`/`.ret`; keeping the
/// shape the same means only the three literal sites in
/// `datalink-extcore/src/declare.rs` need updating.
#[derive(Clone, Debug)]
pub struct FnDecl {
    /// The SQL-visible function name (e.g. `"aba_validate"`).
    pub name: &'static str,
    /// The capability kind ([`CapabilityKind::Scalar`],
    /// [`CapabilityKind::Aggregate`] or [`CapabilityKind::Table`]).
    pub kind: CapabilityKind,
    /// The neutral argument types, in order.
    pub args: &'static [NeutralType],
    /// The neutral return type. For [`CapabilityKind::Table`] this is
    /// unused (the per-column types live in `columns`); by convention the
    /// codegen sets it to [`NeutralType::Complex`] with a `"TABLE"` tag.
    pub ret: NeutralType,
    /// The NULL-argument contract.
    pub null_handling: NullHandling,
    /// Whether the function is deterministic (same input → same output).
    /// Maps to DuckDB `Funcflags::DETERMINISTIC` / SQLite
    /// `FunctionFlags::DETERMINISTIC`.
    pub deterministic: bool,
    /// Output columns for a [`CapabilityKind::Table`] entry, in declared
    /// order. Empty for scalar/aggregate.
    pub columns: &'static [ColDecl],
    /// For a [`CapabilityKind::Table`] entry: file extensions (no dot,
    /// lower-case, e.g. `"gb"`, `"gbk"`) the shim should wire to a
    /// DuckDB replacement scan for this table function. Empty means "no
    /// replacement scan"; consuming shims that do not support this feature
    /// (e.g. `sqlite_shim!`) ignore it.
    pub replacement_scan_extensions: &'static [&'static str],
}
