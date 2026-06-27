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
}

/// One declared function in an extension's capability table. A
/// [`slice`](core::slice) of these IS the extension's neutral surface;
/// both per-DB shims are derived from it, so surface drift between the
/// two databases becomes structurally impossible.
#[derive(Clone, Debug)]
pub struct FnDecl {
    /// The SQL-visible function name (e.g. `"aba_validate"`).
    pub name: &'static str,
    /// The capability kind. Currently always [`CapabilityKind::Scalar`].
    pub kind: CapabilityKind,
    /// The neutral argument types, in order.
    pub args: &'static [NeutralType],
    /// The neutral return type.
    pub ret: NeutralType,
    /// The NULL-argument contract.
    pub null_handling: NullHandling,
    /// Whether the function is deterministic (same input → same output).
    /// Maps to DuckDB `Funcflags::DETERMINISTIC` / SQLite
    /// `FunctionFlags::DETERMINISTIC`.
    pub deterministic: bool,
}
