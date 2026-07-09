//! Dynlink-mode sqlite-target emitter for the shim-bridge codegen.
//!
//! Phase A sibling of `datalink-shim-duckdb-dynlink-emit`. Consumes
//! a `<extension>-catalog.toml` extension surface (leaves or umbrellas)
//! and emits a `wasm32-wasip2` cdylib bridge crate whose component:
//!
//!   * imports `compose:dynlink/linker@0.1.0` for outbound dispatch
//!     to a resident provider (`opts.provider_id`);
//!   * exports the canonical `sqlite:extension` contract surface
//!     (extension + extension-callbacks) so the sqlink host binds
//!     against the composite world without a missing-export failure.
//!
//! Following §A.4 Option 1 of the Spatial-Catalog Integration
//! design, every scalar arm is opaque: params ferried as CBOR
//! blobs, returns unwrapped as CBOR blobs / primitives, all type
//! inference owned by the resident provider. The bridge is a pure
//! CBOR tunnel.
//!
//! Aggregate / collation / hook exports are wired as
//! error-returning stubs at Phase A scope; scalar registration is
//! the only path currently routed to `resolve-by-id + invoke`.
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
//! wit/deps/sqlite-extension/*.wit  (copied from ~/git/sqlink/wit)
//! ```

pub mod emit_dynlink;
pub mod sql_extension_catalog;

pub use emit_dynlink::emit_dynlink;

use std::path::Path;

use anyhow::{Context, Result};

/// Options for `emit`.
///
/// * `provider_id` — the id the emitted bridge resolves at
///   instantiate time via `compose:dynlink/linker.resolve-by-id`.
///   Matches what the host's process-global dynlink provider
///   registry hands out for the composed sub-extension.
/// * `sub_ext` — SQL-facing sub-extension name (e.g. `postgis_core`).
///   Used for the crate name + diagnostic prefixes.
/// * `extension_root` — extension root (`postgis`) — used in
///   package names, provider-crate lookup, and README text.
/// * `target` — leaf id or umbrella id from the catalog. Determines
///   which functions the emitted bridge advertises + dispatches.
pub struct DynlinkOptions {
    pub provider_id: String,
    pub sub_ext: String,
    pub extension_root: String,
    pub target: String,
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
    let catalog = sql_extension_catalog::load(catalog_toml)
        .with_context(|| format!("loading extension catalog: {}", catalog_toml.display()))?;
    emit_dynlink(&catalog, None, out_dir, &opts)
}

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn crate_symbols_are_public() {
        // Compile-only smoke: ensure the public API items are
        // reachable through the crate root. If someone accidentally
        // makes `emit_dynlink` or `DynlinkOptions` non-public, this
        // fails to compile.
        let _: fn(&Path, &str, &Path, DynlinkOptions) -> anyhow::Result<()> = emit;
        let _ = DynlinkOptions {
            provider_id: "id".into(),
            sub_ext: "sub".into(),
            extension_root: "root".into(),
            target: "t".into(),
        };
    }
}
