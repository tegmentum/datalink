//! Static literal blocks emitted into the bridge crate's
//! `src/lib.rs` to materialize the runtime `u32 -> scalar-index`
//! handle table.
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
//! Two globals:
//!   * `NEXT_HANDLE: AtomicU32` — monotonic counter handed out at
//!     `register_scalars()` time.
//!   * `handle_table() -> &'static Mutex<HashMap<u32, usize>>` — a
//!     `usize` index into the bridge's compile-time `SCALAR_ARMS`
//!     match. Each scalar's match arm gets its own index; the
//!     callback handle the host receives at register time stays
//!     stable for the extension's lifetime.

/// Emit the handle-table literal block. Goes at the top of the
/// generated `src/lib.rs` immediately after the prelude, before
/// the `impl callback_dispatch::Guest` block that reads from it.
pub fn render() -> &'static str {
    HANDLE_TABLE_BLOCK
}

const HANDLE_TABLE_BLOCK: &str = r##"// ─── Handle table (u32 -> scalar-arm index) ───
//
// `register_scalars()` allocates one handle per scalar and stores
// the arm index. `call_scalar` looks up the index and dispatches
// via the generated SCALAR_ARMS match.

fn handle_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, usize>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

static NEXT_HANDLE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1);

"##;
