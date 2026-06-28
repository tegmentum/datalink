//! `sqlite_agg_shim!`: generate the sqlink (`sqlite:extension`) WINDOW /
//! aggregate shim from an [`ExtCore`](crate::ExtCore) that declares
//! aggregates (optionally alongside scalars).
//!
//! This is the window-tier sibling of [`sqlite_shim!`](crate::sqlite_shim)
//! (which is scalar-only) and the SQLite counterpart of
//! [`duckdb_agg_shim!`](crate::duckdb_agg_shim). It targets the
//! `sqlite:extension/stateful` world, which exports `aggregate-function`
//! (step / finalize / value / inverse) in addition to `scalar-function`
//! and `metadata`.
//!
//! # The window ABI it targets
//!
//! SQLite drives a window function through `create_window_function`'s four
//! slots: `xStep` (a row enters the frame), `xInverse` (a row leaves the
//! frame), `xValue` (the current frame's value, context preserved) and
//! `xFinal` (the final value, context released). The sqlink host's loader
//! ([`sqlink-loader`]) registers any `aggregate-function-spec` whose
//! `is-window` is true via `create_window_function` and routes those four
//! slots to this guest's `step` / `inverse` / `value` / `finalize`.
//!
//! # How the generic window template stays core-agnostic
//!
//! The neutral [`ExtCore::dispatch_aggregate`](crate::ExtCore::dispatch_aggregate)
//! fold is whole-frame (`init -> step* -> finalize` over the rows of the
//! current frame). SQLite's `xInverse` is incremental, but a core need not
//! supply an inverse: this shim keeps the current frame's rows in a
//! per-`context-id` buffer (FIFO; `step` pushes, `inverse` pops the
//! oldest) and RE-RUNS the whole-frame fold for each `value()` / final.
//! So the exact same `aggregate` declaration that drives the DuckDB
//! `call-aggregate-window` path also drives the SQLite streaming path —
//! write once, both window engines.
//!
//! The per-context buffers live in a `thread_local`; the sqlink host
//! caches the stateful Store across the step/value/inverse/finalize calls
//! of one aggregation (the same contract `stats`/`count_min` rely on), so
//! the buffer survives the frame's lifetime.
//!
//! # Marshalling + NULL handling
//!
//! Identical to [`sqlite_shim!`]: the closed FROZEN value set plus the
//! `complex()` / `wit-value` escape hatch ONLY. Scalar NULL propagation is
//! enforced here (SQLite has no host-side default). Aggregate rows are fed
//! verbatim — including NULL rows, so the FIFO `inverse` pop stays aligned
//! with SQLite's step/inverse pairing; the core's `step` decides what to
//! skip.

/// Expand the full sqlink window/aggregate shim for `$core`.
///
/// ```ignore
/// sqlite_agg_shim! {
///     core = talib_core::Core;
///     bindings = bindings;
///     types = bindings::sqlite::extension::types;
///     metadata = bindings::exports::sqlite::extension::metadata;
///     scalar_function = bindings::exports::sqlite::extension::scalar_function;
///     aggregate_function = bindings::exports::sqlite::extension::aggregate_function;
///     prefix_expansion = "com.tegmentum.sqlink.ext.talib";
/// }
/// ```
#[macro_export]
macro_rules! sqlite_agg_shim {
    (
        core = $core:path ;
        bindings = $bindings:path ;
        types = $types:path ;
        metadata = $meta:path ;
        scalar_function = $sf:path ;
        aggregate_function = $af:path ;
        prefix_expansion = $prefix_exp:expr ;
    ) => {
        const _: () = {
            use $crate::ExtCore as _;
            use $bindings as __bindings;
            use $types as __types;
            use $meta as __meta;
            use $sf as __sf;
            use $af as __af;
            use __types::{FunctionFlags, SqlValue};
            use __meta::{
                AggregateFunctionSpec, Guest as MetadataGuest, Manifest, ScalarFunctionSpec,
            };
            use __sf::Guest as ScalarFunctionGuest;
            use __af::Guest as AggregateFunctionGuest;

            type Core = $core;

            struct Ext;

            // ---- Marshalling: SqlValue <-> NeutralValue (== sqlite_shim!) ----

            fn to_neutral(v: &SqlValue) -> $crate::NeutralValue {
                match v {
                    SqlValue::Null => $crate::NeutralValue::Null,
                    SqlValue::Integer(n) => $crate::NeutralValue::Int64(*n),
                    SqlValue::Real(f) => $crate::NeutralValue::Float64(*f),
                    SqlValue::Text(s) => {
                        $crate::NeutralValue::Text(::std::string::String::from(s.as_str()))
                    }
                    SqlValue::Blob(b) => $crate::NeutralValue::Blob(b.clone()),
                    SqlValue::WitValue(p) => $crate::NeutralValue::Complex {
                        type_expr: ::std::string::String::from(p.symbolic_name.as_str()),
                        json: ::std::string::String::new(),
                    },
                }
            }

            fn from_neutral(v: $crate::NeutralValue) -> SqlValue {
                match v {
                    $crate::NeutralValue::Null => SqlValue::Null,
                    $crate::NeutralValue::Boolean(b) => SqlValue::Integer(b as i64),
                    $crate::NeutralValue::Int64(n) => SqlValue::Integer(n),
                    $crate::NeutralValue::Float64(f) => SqlValue::Real(f),
                    $crate::NeutralValue::Text(s) => SqlValue::Text(s),
                    $crate::NeutralValue::Blob(b) => SqlValue::Blob(b),
                    $crate::NeutralValue::Complex { type_expr, json } => {
                        SqlValue::WitValue(__types::WitValuePayload {
                            type_id: ::std::vec::Vec::new(),
                            bytes: json.into_bytes(),
                            symbolic_name: type_expr,
                        })
                    }
                }
            }

            // ---- MetadataGuest::describe -> Manifest ----
            //
            // func-ids are 1-based in DECLS order, so `func_id - 1` is the
            // DECLS index for BOTH scalar dispatch and aggregate dispatch.

            impl MetadataGuest for Ext {
                fn describe() -> Manifest {
                    let mut scalar_functions = ::std::vec::Vec::new();
                    let mut aggregate_functions = ::std::vec::Vec::new();
                    for (idx, decl) in <Core as $crate::ExtCore>::DECLS.iter().enumerate() {
                        let mut func_flags = FunctionFlags::empty();
                        if decl.deterministic {
                            func_flags |= FunctionFlags::DETERMINISTIC;
                        }
                        let id = (idx as u64) + 1;
                        match decl.kind {
                            $crate::CapabilityKind::Scalar => {
                                scalar_functions.push(ScalarFunctionSpec {
                                    id,
                                    name: decl.name.into(),
                                    num_args: decl.args.len() as i32,
                                    func_flags,
                                });
                            }
                            $crate::CapabilityKind::Aggregate => {
                                aggregate_functions.push(AggregateFunctionSpec {
                                    id,
                                    name: decl.name.into(),
                                    num_args: decl.args.len() as i32,
                                    func_flags,
                                    // Every declared aggregate is offered as
                                    // a window function too; the generic
                                    // buffer below supplies value()+inverse().
                                    is_window: true,
                                });
                            }
                        }
                    }
                    Manifest {
                        name: <Core as $crate::ExtCore>::NAME.into(),
                        version: <Core as $crate::ExtCore>::VERSION.into(),
                        scalar_functions,
                        aggregate_functions,
                        collations: ::std::vec::Vec::new(),
                        vtabs: ::std::vec::Vec::new(),
                        has_authorizer: false,
                        has_update_hook: false,
                        has_commit_hook: false,
                        has_wal_hook: false,
                        wal_hook_id: 0,
                        dot_commands: ::std::vec::Vec::new(),
                        declared_capabilities: ::std::vec::Vec::new(),
                        optional_capabilities: ::std::vec::Vec::new(),
                        preferred_prefix: Some(<Core as $crate::ExtCore>::NAME.into()),
                        prefix_expansion: Some($prefix_exp.into()),
                        typed_values: ::std::vec::Vec::new(),
                    }
                }
            }

            // ---- ScalarFunctionGuest::call (== sqlite_shim!) ----

            impl ScalarFunctionGuest for Ext {
                fn call(
                    func_id: u64,
                    args: ::std::vec::Vec<SqlValue>,
                ) -> Result<SqlValue, ::std::string::String> {
                    if func_id == 0 {
                        return Err(::std::format!(
                            "{}: invalid func id 0",
                            <Core as $crate::ExtCore>::NAME
                        ));
                    }
                    let idx = (func_id - 1) as usize;
                    let decl = <Core as $crate::ExtCore>::DECLS.get(idx).ok_or_else(|| {
                        ::std::format!(
                            "{}: unknown func id {}",
                            <Core as $crate::ExtCore>::NAME,
                            func_id
                        )
                    })?;
                    let neutral: ::std::vec::Vec<$crate::NeutralValue> =
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

            // ---- AggregateFunctionGuest: the generic window template ----
            //
            // One per-context FIFO buffer of the frame's rows (each row =
            // that step's neutral args). value()/finalize() re-run the
            // whole-frame neutral fold over the current buffer.

            thread_local! {
                static FRAMES: ::std::cell::RefCell<
                    ::std::collections::HashMap<
                        u64,
                        ::std::vec::Vec<::std::vec::Vec<$crate::NeutralValue>>,
                    >,
                > = ::std::cell::RefCell::new(::std::collections::HashMap::new());
            }

            fn fold_frame(
                func_id: u64,
                buffer: &[::std::vec::Vec<$crate::NeutralValue>],
            ) -> Result<SqlValue, ::std::string::String> {
                if func_id == 0 {
                    return Err(::std::format!(
                        "{}: invalid aggregate func id 0",
                        <Core as $crate::ExtCore>::NAME
                    ));
                }
                let idx = (func_id - 1) as usize;
                let row_refs: ::std::vec::Vec<&[$crate::NeutralValue]> =
                    buffer.iter().map(|r| r.as_slice()).collect();
                let res = <Core as $crate::ExtCore>::dispatch_aggregate(idx, &row_refs)?;
                Ok(from_neutral(res))
            }

            impl AggregateFunctionGuest for Ext {
                fn step(
                    _func_id: u64,
                    context_id: u64,
                    args: ::std::vec::Vec<SqlValue>,
                ) -> Result<(), ::std::string::String> {
                    let neutral: ::std::vec::Vec<$crate::NeutralValue> =
                        args.iter().map(to_neutral).collect();
                    FRAMES.with(|f| {
                        f.borrow_mut().entry(context_id).or_default().push(neutral);
                    });
                    Ok(())
                }

                fn finalize(
                    func_id: u64,
                    context_id: u64,
                ) -> Result<SqlValue, ::std::string::String> {
                    let buffer = FRAMES
                        .with(|f| f.borrow_mut().remove(&context_id))
                        .unwrap_or_default();
                    fold_frame(func_id, &buffer)
                }

                fn value(
                    func_id: u64,
                    context_id: u64,
                ) -> Result<SqlValue, ::std::string::String> {
                    FRAMES.with(|f| {
                        let map = f.borrow();
                        let empty = ::std::vec::Vec::new();
                        let buffer = map.get(&context_id).unwrap_or(&empty);
                        fold_frame(func_id, buffer)
                    })
                }

                fn inverse(
                    _func_id: u64,
                    context_id: u64,
                    _args: ::std::vec::Vec<SqlValue>,
                ) -> Result<(), ::std::string::String> {
                    // The oldest row leaves the frame (FIFO): pop the front.
                    FRAMES.with(|f| {
                        if let Some(buffer) = f.borrow_mut().get_mut(&context_id) {
                            if !buffer.is_empty() {
                                buffer.remove(0);
                            }
                        }
                    });
                    Ok(())
                }
            }

            __bindings::export!(Ext with_types_in __bindings);
        };
    };
}
