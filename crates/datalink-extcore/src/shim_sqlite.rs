//! `sqlite_shim!`: generate the sqlink (`sqlite:extension`) component
//! shim from an [`ExtCore`](crate::ExtCore).
//!
//! Faithful TRANSCRIPTION of the hand-written glue in sqlink
//! `extensions/aba/src/lib.rs` — the `MetadataGuest::describe()` →
//! `Manifest` (the full field set), the `ScalarFunctionGuest::call`
//! func-id dispatch, and the `SqlValue` marshalling — generalized over
//! `Core::DECLS`. Func-ids are `1..=N` in declaration order.
//!
//! # Parameterization
//!
//! The consuming crate runs its own `wit_bindgen::generate!` (sqlink:
//! `sqlite:extension@1.0.0`, wit-bindgen 0.44, world `minimal`) and
//! passes the binding paths in; nothing here hardcodes the
//! package/version.
//!
//! # SQLite marshalling conventions (vs DuckDB)
//!
//!   * BOOLEAN: SQLite has no native boolean — a neutral
//!     [`Boolean`](crate::NeutralValue::Boolean) marshals to
//!     `SqlValue::Integer(0|1)` (DuckDB uses `Duckvalue::Boolean`).
//!   * NULL: no host-side default propagation, so the generated dispatch
//!     enforces [`NullHandling::Propagate`](crate::NullHandling) itself.
//!   * NULL-on-error / BLOB<->TEXT subtleties live in the core body
//!     (e.g. baseN decode returns [`NeutralValue::Null`]).

/// Expand the full sqlink component shim for `$core`.
///
/// ```ignore
/// sqlite_shim! {
///     core = aba_core::Core;
///     bindings = bindings;             // the module wrapping generate!
///     types = bindings::sqlite::extension::types;
///     metadata = bindings::exports::sqlite::extension::metadata;
///     scalar_function = bindings::exports::sqlite::extension::scalar_function;
///     prefix_expansion = "com.tegmentum.sqlink.ext.aba";
/// }
/// ```
#[macro_export]
macro_rules! sqlite_shim {
    (
        core = $core:path ;
        bindings = $bindings:path ;
        types = $types:path ;
        metadata = $meta:path ;
        scalar_function = $sf:path ;
        prefix_expansion = $prefix_exp:expr ;
    ) => {
        const _: () = {
            use $crate::ExtCore as _;
            // Alias each binding MODULE (a `:path` fragment cannot be
            // followed by `::{...}` in a `use`, so import members off the
            // alias instead). This keeps the macro parameterized by the
            // consuming repo's WIT paths with no `::{}` after a fragment.
            use $bindings as __bindings;
            use $types as __types;
            use $meta as __meta;
            use $sf as __sf;
            use __types::{FunctionFlags, SqlValue};
            use __meta::{Guest as MetadataGuest, Manifest, ScalarFunctionSpec};
            use __sf::Guest as ScalarFunctionGuest;

            type Core = $core;

            struct Ext;

            // ---- Marshalling: SqlValue <-> NeutralValue ----

            fn to_neutral(v: &SqlValue) -> $crate::NeutralValue {
                match v {
                    SqlValue::Null => $crate::NeutralValue::Null,
                    // SQLite has no boolean; integers carry it.
                    SqlValue::Integer(n) => $crate::NeutralValue::Int64(*n),
                    SqlValue::Real(f) => $crate::NeutralValue::Float64(*f),
                    SqlValue::Text(s) => {
                        $crate::NeutralValue::Text(::alloc::string::String::from(s.as_str()))
                    }
                    SqlValue::Blob(b) => $crate::NeutralValue::Blob(b.clone()),
                    // The wit-value escape arm maps to the neutral Complex
                    // escape hatch (symbolic-name -> type-expr). Decoding
                    // the CBOR payload is the core's job if it declares it;
                    // for the FROZEN set we surface it as opaque.
                    SqlValue::WitValue(p) => $crate::NeutralValue::Complex {
                        type_expr: ::alloc::string::String::from(p.symbolic_name.as_str()),
                        json: ::alloc::string::String::new(),
                    },
                }
            }

            fn from_neutral(v: $crate::NeutralValue) -> SqlValue {
                match v {
                    $crate::NeutralValue::Null => SqlValue::Null,
                    // BOOLEAN convention: DuckDB Boolean -> SQLite Integer.
                    $crate::NeutralValue::Boolean(b) => SqlValue::Integer(b as i64),
                    $crate::NeutralValue::Int64(n) => SqlValue::Integer(n),
                    $crate::NeutralValue::Float64(f) => SqlValue::Real(f),
                    $crate::NeutralValue::Text(s) => SqlValue::Text(s),
                    $crate::NeutralValue::Blob(b) => SqlValue::Blob(b),
                    // Composite results ride the wit-value escape arm. The
                    // FROZEN-set rule: never add a native sql-value arm.
                    $crate::NeutralValue::Complex { type_expr, json } => {
                        SqlValue::WitValue(__types::WitValuePayload {
                            type_id: ::alloc::vec::Vec::new(),
                            bytes: json.into_bytes(),
                            symbolic_name: type_expr,
                        })
                    }
                }
            }

            // ---- MetadataGuest::describe -> Manifest ----

            impl MetadataGuest for Ext {
                fn describe() -> Manifest {
                    let mut scalar_functions = ::alloc::vec::Vec::new();
                    for (idx, decl) in
                        <Core as $crate::ExtCore>::DECLS.iter().enumerate()
                    {
                        let mut func_flags = FunctionFlags::empty();
                        if decl.deterministic {
                            func_flags |= FunctionFlags::DETERMINISTIC;
                        }
                        scalar_functions.push(ScalarFunctionSpec {
                            // func-id is 1-based in declaration order.
                            id: (idx as u64) + 1,
                            name: decl.name.into(),
                            num_args: decl.args.len() as i32,
                            func_flags,
                        });
                    }
                    Manifest {
                        name: <Core as $crate::ExtCore>::NAME.into(),
                        version: <Core as $crate::ExtCore>::VERSION.into(),
                        scalar_functions,
                        aggregate_functions: ::alloc::vec::Vec::new(),
                        collations: ::alloc::vec::Vec::new(),
                        vtabs: ::alloc::vec::Vec::new(),
                        has_authorizer: false,
                        has_update_hook: false,
                        has_commit_hook: false,
                        has_wal_hook: false,
                        wal_hook_id: 0,
                        dot_commands: ::alloc::vec::Vec::new(),
                        declared_capabilities: ::alloc::vec::Vec::new(),
                        optional_capabilities: ::alloc::vec::Vec::new(),
                        preferred_prefix: Some(
                            <Core as $crate::ExtCore>::NAME.into(),
                        ),
                        prefix_expansion: Some($prefix_exp.into()),
                        typed_values: ::alloc::vec::Vec::new(),
                    }
                }
            }

            // ---- ScalarFunctionGuest::call ----

            impl ScalarFunctionGuest for Ext {
                fn call(
                    func_id: u64,
                    args: ::alloc::vec::Vec<SqlValue>,
                ) -> Result<SqlValue, ::alloc::string::String> {
                    if func_id == 0 {
                        return Err(::alloc::format!(
                            "{}: invalid func id 0",
                            <Core as $crate::ExtCore>::NAME
                        ));
                    }
                    let idx = (func_id - 1) as usize;
                    let decl = <Core as $crate::ExtCore>::DECLS.get(idx).ok_or_else(
                        || {
                            ::alloc::format!(
                                "{}: unknown func id {}",
                                <Core as $crate::ExtCore>::NAME,
                                func_id
                            )
                        },
                    )?;
                    let neutral: ::alloc::vec::Vec<$crate::NeutralValue> =
                        args.iter().map(to_neutral).collect();
                    if matches!(decl.null_handling, $crate::NullHandling::Propagate)
                        && neutral.iter().any(|v| v.is_null())
                    {
                        return Ok(SqlValue::Null);
                    }
                    let res = <Core as $crate::ExtCore>::dispatch(idx, &neutral)?;
                    Ok(from_neutral(res))
                }
            }

            __bindings::export!(Ext with_types_in __bindings);
        };
    };
}
