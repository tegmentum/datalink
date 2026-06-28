//! SQLite-target emitter of the shim-bridge codegen.
//!
//! Step 3 of PLAN-shim-codegen-datalink-migration lifts the
//! `emit_sqlite/` subtree of `sqlink-shim-codegen` into this crate
//! so the database-agnostic substrate (`datalink-shim-codegen-core`)
//! and the per-database emitter are clearly separated. The CLI in
//! `sqlink-shim-codegen` becomes a thin wrapper around this crate.
//!
//! Produces a Rust crate compilable for `wasm32-wasip2` as a
//! `cdylib`. The component imports the upstream shim's WIT
//! interfaces and exports the canonical `sqlite:extension/minimal`-
//! shape contract (metadata + scalar-function + aggregate-function
//! + vtab).
//!
//! Checkpoint 3.1: scaffold only — modules + `emit()` entry point
//! land in checkpoint 3.2.

// Touch the upstream deps so the cargo check at scaffold time
// exercises the dependency graph. Removed in checkpoint 3.2 when
// the real module declarations land.
#[allow(unused_imports)]
use datalink_shim_codegen_core as _datalink_substrate;
#[allow(unused_imports)]
use shim_bridge_codegen_core as _shim_plan;
