//! `embed_shim!`: generate sqlink's STATIC embed path from an
//! [`ExtCore`](crate::ExtCore).
//!
//! Faithful TRANSCRIPTION of the hand-written glue in sqlink
//! `extensions/aba/src/embed.rs` — the `call_scalar(func_id, args)` over
//! `SqlValueOwned`, the `ScalarSpec` table, and `register_into` — driven
//! by `Core::DECLS`. This is the THIRD generated target the design calls
//! out: it kills sqlink's embed-path triplication (the same algorithm
//! was maintained a third time here).
//!
//! The embed path links the extension's scalars directly into the CLI
//! via `sqlite3_create_function_v2` (no WIT, no component), so it uses
//! `sqlite_embed::{SqlValueOwned, ScalarSpec, register_scalars}` rather
//! than the WIT `SqlValue`. The marshalling conventions are identical to
//! [`sqlite_shim!`](crate::sqlite_shim) (boolean -> integer, etc.).

/// Expand the embed path for `$core`.
///
/// ```ignore
/// embed_shim! {
///     core = aba_core::Core;
///     sqlite_embed = sqlite_embed;     // the sqlite-embed crate
/// }
/// ```
///
/// Produces `pub fn call_scalar(...)` and
/// `pub unsafe fn register_into(db) -> c_int` in the enclosing module
/// (matching the hand-written `embed.rs` public surface the CLI links).
#[macro_export]
macro_rules! embed_shim {
    (
        core = $core:path ;
        sqlite_embed = $se:path ;
    ) => {
        use $se::{register_scalars as __dl_register_scalars, ScalarSpec, SqlValueOwned};

        type EmbedCore = $core;

        fn __dl_to_neutral(v: &SqlValueOwned) -> $crate::NeutralValue {
            match v {
                SqlValueOwned::Null => $crate::NeutralValue::Null,
                SqlValueOwned::Integer(n) => $crate::NeutralValue::Int64(*n),
                SqlValueOwned::Real(f) => $crate::NeutralValue::Float64(*f),
                SqlValueOwned::Text(s) => {
                    $crate::NeutralValue::Text(::alloc::string::String::from(s.as_str()))
                }
                SqlValueOwned::Blob(b) => $crate::NeutralValue::Blob(b.clone()),
            }
        }

        fn __dl_from_neutral(
            v: $crate::NeutralValue,
        ) -> ::core::result::Result<SqlValueOwned, ::alloc::string::String> {
            ::core::result::Result::Ok(match v {
                $crate::NeutralValue::Null => SqlValueOwned::Null,
                // BOOLEAN convention: DuckDB Boolean -> SQLite Integer.
                $crate::NeutralValue::Boolean(b) => SqlValueOwned::Integer(b as i64),
                $crate::NeutralValue::Int64(n) => SqlValueOwned::Integer(n),
                $crate::NeutralValue::Float64(f) => SqlValueOwned::Real(f),
                $crate::NeutralValue::Text(s) => SqlValueOwned::Text(s),
                $crate::NeutralValue::Blob(b) => SqlValueOwned::Blob(b),
                // The embed path has no escape arm; a composite result is
                // an extension-author error, surfaced rather than dropped.
                $crate::NeutralValue::Complex { type_expr, .. } => {
                    return ::core::result::Result::Err(::alloc::format!(
                        "{}: composite result type '{}' not supported on the embed path",
                        <EmbedCore as $crate::ExtCore>::NAME,
                        type_expr
                    ))
                }
            })
        }

        /// Embed-path scalar dispatch. Mirrors the WIT `call`: func-id is
        /// 1-based in `DECLS` order.
        pub fn call_scalar(
            func_id: u64,
            args: ::alloc::vec::Vec<SqlValueOwned>,
        ) -> ::core::result::Result<SqlValueOwned, ::alloc::string::String> {
            if func_id == 0 {
                return ::core::result::Result::Err(::alloc::format!(
                    "{}: invalid func id 0",
                    <EmbedCore as $crate::ExtCore>::NAME
                ));
            }
            let idx = (func_id - 1) as usize;
            let decl = <EmbedCore as $crate::ExtCore>::DECLS.get(idx).ok_or_else(|| {
                ::alloc::format!(
                    "{}: unknown func id {}",
                    <EmbedCore as $crate::ExtCore>::NAME,
                    func_id
                )
            })?;
            let neutral: ::alloc::vec::Vec<$crate::NeutralValue> =
                args.iter().map(__dl_to_neutral).collect();
            if matches!(decl.null_handling, $crate::NullHandling::Propagate)
                && neutral.iter().any(|v| v.is_null())
            {
                return ::core::result::Result::Ok(SqlValueOwned::Null);
            }
            let res = <EmbedCore as $crate::ExtCore>::dispatch(idx, &neutral)?;
            __dl_from_neutral(res)
        }

        /// Register every declared scalar into `db`. Builds the
        /// `ScalarSpec` table from `DECLS` + the compile-time
        /// NUL-terminated names.
        pub unsafe fn register_into(
            db: *mut ::libsqlite3_sys::sqlite3,
        ) -> ::core::ffi::c_int {
            // T5: only Scalar decls belong on the embed path (aggregates
            // stay on the WIT stateful shim; tables ride the sqlite vtab
            // shape, unwritten here). Filter defensively so a mixed core
            // still links its scalars.
            let specs: ::alloc::vec::Vec<ScalarSpec> =
                <EmbedCore as $crate::ExtCore>::DECLS
                    .iter()
                    .enumerate()
                    .filter(|(_, decl)| {
                        matches!(decl.kind, $crate::CapabilityKind::Scalar)
                    })
                    .map(|(idx, decl)| ScalarSpec {
                        func_id: (idx as u64) + 1,
                        name: <EmbedCore>::SCALAR_NAMES_NUL[idx],
                        num_args: decl.args.len() as i32,
                        deterministic: decl.deterministic,
                    })
                    .collect();
            __dl_register_scalars(db, &specs, call_scalar)
        }
    };
}
