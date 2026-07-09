//! Emit the `guest::Guest` lifecycle impl block.
//!
//! `duckdb:extension/guest` has three functions:
//!
//!   * `load() -> result<loadresult, duckerror>` — invoked once by
//!     the host after instantiation; registers every scalar /
//!     aggregate / table function the bridge exposes against the
//!     runtime's `runtime::get_capability(...)` callbacks, then
//!     returns a `Loadresult` carrying the bridge's name, version,
//!     and required capabilities.
//!   * `reconfigure(keys: list<string>) -> result<bool, duckerror>`
//!     — no-op for the scalar-first cut (returns `false` to signal
//!     no reconfiguration done).
//!   * `shutdown() -> result<bool, duckerror>` — no-op for the
//!     scalar-first cut (returns `false`).
//!
//! Mirrors the `impl guest::Guest` block in `shim_duckdb.rs`'s
//! macro expansion. The differences are surface-level (a generated
//! struct name instead of `Extension`; explicit String literals
//! pulled from the BridgePlan instead of `<Core as ExtCore>::NAME`).

use shim_bridge_codegen_core::BridgePlan;

/// Render the lifecycle impl block. `bridge_struct` is the
/// PascalCase struct name the bridge crate exports (e.g.
/// `PostgisBridge`); `primary` is the SQL-side extension name
/// (e.g. `postgis`); `version` is the extension version string.
///
/// `has_aggregates` / `has_tables` gate the `register_aggregates()` /
/// `register_tables()` calls so an empty surface (no IR entries)
/// doesn't drag in a registry-fetch that the host might not
/// expose. The emit always emits the body functions; lifecycle::
/// load is what decides whether to call them.
pub fn render(
    bridge_struct: &str,
    plan: &BridgePlan,
    has_aggregates: bool,
    has_tables: bool,
    has_casts: bool,
    has_windows: bool,
    has_logical_types: bool,
) -> String {
    let primary = plan
        .extensions
        .first()
        .map(|e| e.name.as_str())
        .unwrap_or("shim");
    let version = plan
        .extensions
        .first()
        .map(|e| e.version.as_str())
        .unwrap_or("0.1.0");
    // #64 / #67: register logical types (GEOMETRY / GEOGRAPHY / etc.)
    // and their implicit BLOB casts BEFORE any function registration so
    // the DuckDB binder can resolve `st_area(GEOMETRY)` -> `st_area(BLOB)`
    // via the implicit cast when scalar_registry.register runs.
    let logical_type_call = if has_logical_types {
        "        register_logical_types()?;\n"
    } else {
        ""
    };
    let aggregate_call = if has_aggregates {
        "        register_aggregates()?;\n"
    } else {
        ""
    };
    let table_call = if has_tables {
        "        register_tables()?;\n"
    } else {
        ""
    };
    let cast_call = if has_casts {
        "        register_casts()?;\n"
    } else {
        ""
    };
    let window_call = if has_windows {
        "        register_windows()?;\n"
    } else {
        ""
    };
    format!(
        r##"
impl guest::Guest for {bridge_struct} {{
    fn load() -> Result<types::Loadresult, types::Duckerror> {{
{logical_type_call}        register_scalars()?;
        // #65: every scalar is also registered as a single-row TVF
        // so DuckDB's FROM-clause dispatcher can find it. Emission
        // is unconditional because every bridge has scalars; the
        // emitted body probes for the table capability and skips
        // (with a diagnostic) when it isn't advertised.
        register_scalar_tvfs()?;
{aggregate_call}{table_call}{cast_call}{window_call}        Ok(types::Loadresult {{
            name: "{primary}".into(),
            version: Some("{version}".into()),
            requires: Vec::new().into(),
        }})
    }}
    fn reconfigure(
        _keys: Vec<String>,
    ) -> Result<bool, types::Duckerror> {{
        Ok(false)
    }}
    fn shutdown() -> Result<bool, types::Duckerror> {{
        Ok(false)
    }}
}}
"##,
    )
}
