//! Static literal blocks emitted into the bridge crate's
//! `src/lib.rs` to materialize the runtime `u32 -> arm-index`
//! handle tables (one per dispatch family).
//!
//! ## Why this is per-bridge code
//!
//! `datalink-extcore`'s `duckdb_shim!` macro (the precedent in
//! `shim_duckdb.rs`) materializes a `static T: OnceLock<...>`
//! inside the macro-expanded scope so each compiled extension
//! gets its own table. The codegen here mirrors that pattern but
//! at codegen time: the literal block becomes part of the
//! generated `src/lib.rs` so wit-bindgen's `export!(...)` and the
//! callback-dispatch arms can look up their owning function by
//! handle.
//!
//! ## Shape
//!
//! One global counter + per-family tables:
//!   * `NEXT_HANDLE: AtomicU32` — monotonic counter handed out at
//!     `register_*()` time; shared across families so each handle
//!     is globally unique. The DuckDB host gives each registry its
//!     own `u32` namespace at the contract level, but since each
//!     family's `call_*` arm only consults its own table, the
//!     globally-unique counter keeps the registration code uniform.
//!   * `handle_table() -> &'static Mutex<HashMap<u32, usize>>` — a
//!     `usize` index into the bridge's compile-time `SCALAR_ARMS`
//!     match. Used by `call_scalar` / `call_scalar_batch`.
//!   * `aggregate_handle_table() -> &'static Mutex<HashMap<u32, usize>>`
//!     — `u32` aggregate handle → arm index into the aggregate
//!     dispatch match. Used by `call_aggregate`.
//!   * `table_handle_table() -> &'static Mutex<HashMap<u32, usize>>`
//!     — `u32` table-function handle → arm index into the UDTF
//!     dispatch match. Used by `call_table`.
//!
//! Each family has its own arm-index space starting at 0; the
//! handles themselves are namespaced by which table they live in.

/// Emit the handle-table literal block. Goes at the top of the
/// generated `src/lib.rs` immediately after the prelude, before
/// the `impl callback_dispatch::Guest` block that reads from it.
pub fn render() -> &'static str {
    HANDLE_TABLE_BLOCK
}

const HANDLE_TABLE_BLOCK: &str = r##"// ─── Handle tables (u32 -> arm-index, one per dispatch family) ───
//
// `register_scalars()` / `register_aggregates()` / `register_tables()`
// allocate one handle per registration and store its arm index in the
// family-specific table. The corresponding `call_*` dispatch arm
// looks up the index and routes to the per-arm match.

fn handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, usize>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn aggregate_handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, usize>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn table_handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, usize>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

static NEXT_HANDLE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1);

"##;
