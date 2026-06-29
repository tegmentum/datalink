//! Database-agnostic substrate of the shim-bridge codegen.
//!
//! Step 2 of PLAN-shim-codegen-datalink-migration lifts the
//! `core/` subtree of `sqlink-shim-codegen` into this crate so
//! per-database emit (today sqlink's `sqlite:extension`, tomorrow
//! ducklink's `duckdb:extension` and future datafission targets)
//! can consume one shared parser + IR + reachability layer.
//!
//! Every module here is database-agnostic. SqlValue / SqlType /
//! Duckvalue / Logicaltype awareness lives in the per-database
//! emit crates that depend on this one (`datalink-shim-sqlite-emit`,
//! `datalink-shim-duckdb-emit`).

pub mod compose_emit;
pub mod force_link;
pub mod interface_db;
pub mod name_match;
pub mod override_tables;
pub mod record_registry;
pub mod wit_parse;
pub mod wit_paths;
