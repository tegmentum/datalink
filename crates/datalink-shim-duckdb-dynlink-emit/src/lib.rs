//! Dynlink-mode duckdb-target emitter for the shim-bridge codegen.
//!
//! Phase A sibling of `datalink-shim-sqlite-dynlink-emit`. Consumes
//! a `spatial-catalog.toml` extension surface (leaves or umbrellas)
//! and emits a `wasm32-wasip2` cdylib bridge crate whose component:
//!
//!   * imports `compose:dynlink/linker@0.1.0` for outbound dispatch
//!     to a resident provider (`opts.provider_id`);
//!   * exports the canonical `duckdb:extension@4.0.0` guest +
//!     callback-dispatch pair so the ducklink host binds against
//!     the composite world without a missing-export failure.
//!
//! Following §A.4 Option 1 of the Spatial-Catalog Integration
//! design, every scalar arm is opaque: `callback-dispatch::
//! call-scalar` marshals its `duckvalue` args as CBOR values,
//! forwards through `resolve-by-id + invoke`, and re-wraps the
//! response. The columnar hot paths (`call-scalar-batch-col`,
//! `call-aggregate-col`, `call-cast-col`) and every other export
//! (aggregate / cast / table / pragma) are stubbed with
//! `duckerror::internal` at Phase A scope.
//!
//! ## Layout produced under `out_dir`:
//!
//! ```text
//! Cargo.toml
//! README.md
//! src/lib.rs
//! wit/world.wit
//! wit/deps/compose-dynlink/*.wit   (copied from datalink-dynlink)
//! wit/deps/sys-compose/*.wit
//! wit/deps/duckdb/*.wit            (copied from ~/git/ducklink/wit/duckdb-extension)
//! ```

pub mod emit_dynlink;
pub mod spatial_catalog;

pub use emit_dynlink::emit_dynlink;

use std::path::Path;

use anyhow::{Context, Result};

/// Options for `emit`. Mirrors the sibling sqlite-dynlink-emit
/// crate's options struct — every field is target-agnostic.
///
/// `interface_sqlite` is the sibling shim-interface `.sqlite` (e.g.
/// `~/git/postgis-shim-interface/postgis-interface.sqlite`). The
/// catalog TOML only lists scalar names per leaf; per-fn arg types
/// and return type live in the sqlite `scalars` table. When
/// provided, `emit_dynlink` reads that table and emits typed
/// `register_scalar` calls (base `runtime.scalar-registry.register`
/// path). When absent — or a name is missing from the DB — the
/// emitter falls back to a single-arg Blob shape, preserving Phase A
/// behaviour so a codegen invocation without `--interface` still
/// produces a compilable bridge.
pub struct DynlinkOptions {
    pub provider_id: String,
    pub sub_ext: String,
    pub extension_root: String,
    pub target: String,
    pub interface_sqlite: Option<std::path::PathBuf>,
}

/// Public entry point.
///
/// Parses `catalog_toml`, expands `target` into its constituent
/// leaves, then emits a complete bridge crate under `out_dir`.
///
/// The `target` argument is redundant with `opts.target`; when
/// both are set, `opts.target` wins (letting a caller thread the
/// catalog once and vary the target per invocation).
pub fn emit(
    catalog_toml: &Path,
    target: &str,
    out_dir: &Path,
    mut opts: DynlinkOptions,
) -> Result<()> {
    if opts.target.is_empty() {
        opts.target = target.to_string();
    }
    let catalog = spatial_catalog::load(catalog_toml)
        .with_context(|| format!("loading spatial-catalog: {}", catalog_toml.display()))?;
    emit_dynlink(&catalog, None, out_dir, &opts)
}

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn crate_symbols_are_public() {
        let _: fn(&Path, &str, &Path, DynlinkOptions) -> anyhow::Result<()> = emit;
        let _ = DynlinkOptions {
            provider_id: "id".into(),
            sub_ext: "sub".into(),
            extension_root: "root".into(),
            target: "t".into(),
            interface_sqlite: None,
        };
    }
}
