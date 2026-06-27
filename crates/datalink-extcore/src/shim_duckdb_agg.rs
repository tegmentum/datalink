//! `duckdb_agg_shim!`: generate the ducklink (`duckdb:extension`) shim
//! for a core that declares AGGREGATES (optionally alongside scalars).
//!
//! This is the aggregate-tier sibling of [`duckdb_shim!`](crate::duckdb_shim).
//! It is a deliberately SEPARATE macro so [`duckdb_shim!`] — and the wasm
//! bytes of the 31 already-migrated scalar cores — stay byte-for-byte
//! unchanged. Scalar-only cores keep using `duckdb_shim!`; any core with
//! an `aggregate` declaration uses this one (it handles the scalar
//! functions too, e.g. `bloom_contains` next to the `bloom_filter`
//! aggregate).
//!
//! # The aggregate ABI it targets
//!
//! On `duckdb:extension` the host BUFFERS a group's rows and makes a
//! single `call_aggregate(handle, rows)` — `rows: Rowbatch` is the whole
//! group, each inner row the aggregate's args. So the generated dispatch
//! converts the batch to neutral rows and hands them to
//! [`ExtCore::dispatch_aggregate`](crate::ExtCore::dispatch_aggregate),
//! which runs the entire `init` → `step*` → `finalize` fold in-guest. The
//! neutral state is therefore a native Rust value that NEVER marshals
//! across the WIT boundary — there is no partial-state round-trip and no
//! `combine` on this path (the host does no partial aggregation here).
//!
//! # Marshalling + the frozen-type-set rule
//!
//! Identical to [`duckdb_shim!`]: the closed FROZEN value set plus the
//! `complex()` escape hatch ONLY; no new `duckvalue`/`logicaltype` arm is
//! ever introduced. NULL handling for the scalar functions matches
//! `duckdb_shim!` (DuckDB propagates NULL by default). Aggregate NULL/
//! empty-group handling lives in the core's `step`/`finalize`.

/// Expand the full ducklink aggregate-tier shim for `$core`.
///
/// ```ignore
/// duckdb_agg_shim! {
///     core = aggstat_core::Core;
///     types = duckdb::extension::types;
///     runtime = duckdb::extension::runtime;
///     callback_dispatch = exports::duckdb::extension::callback_dispatch;
///     guest = exports::duckdb::extension::guest;
///     export = export;
/// }
/// ```
#[macro_export]
macro_rules! duckdb_agg_shim {
    (
        core = $core:path ;
        types = $types:path ;
        runtime = $rt:path ;
        callback_dispatch = $cbd:path ;
        guest = $guest:path ;
        export = $export:ident ;
    ) => {
        const _: () = {
            use $crate::ExtCore as _;
            use $types as types;
            use $rt as runtime;
            use $cbd as callback_dispatch;
            use $guest as guest;

            type Core = $core;

            struct Extension;

            // ---- Marshalling: Duckvalue <-> NeutralValue ----
            //
            // Closed FROZEN set + the complex() escape hatch ONLY.

            fn to_neutral(v: &types::Duckvalue) -> $crate::NeutralValue {
                match v {
                    types::Duckvalue::Null => $crate::NeutralValue::Null,
                    types::Duckvalue::Boolean(b) => $crate::NeutralValue::Boolean(*b),
                    types::Duckvalue::Int64(n) => $crate::NeutralValue::Int64(*n),
                    types::Duckvalue::Float64(f) => $crate::NeutralValue::Float64(*f),
                    types::Duckvalue::Text(s) => {
                        $crate::NeutralValue::Text(::std::string::String::from(s.as_str()))
                    }
                    types::Duckvalue::Blob(b) => $crate::NeutralValue::Blob(b.clone()),
                    types::Duckvalue::Complex(c) => $crate::NeutralValue::Complex {
                        type_expr: ::std::string::String::from(c.type_expr.as_str()),
                        json: ::std::string::String::from(c.json.as_str()),
                    },
                    other => $crate::NeutralValue::Complex {
                        type_expr: ::std::string::String::from("UNSUPPORTED"),
                        json: ::std::format!("{:?}", other),
                    },
                }
            }

            fn from_neutral(v: $crate::NeutralValue) -> types::Duckvalue {
                match v {
                    $crate::NeutralValue::Null => types::Duckvalue::Null,
                    $crate::NeutralValue::Boolean(b) => types::Duckvalue::Boolean(b),
                    $crate::NeutralValue::Int64(n) => types::Duckvalue::Int64(n),
                    $crate::NeutralValue::Float64(f) => types::Duckvalue::Float64(f),
                    $crate::NeutralValue::Text(s) => types::Duckvalue::Text(s.into()),
                    $crate::NeutralValue::Blob(b) => types::Duckvalue::Blob(b),
                    $crate::NeutralValue::Complex { type_expr, json } => {
                        types::Duckvalue::Complex(types::Complexvalue {
                            type_expr: type_expr.into(),
                            json: json.into(),
                        })
                    }
                }
            }

            fn ntype_to_logical(t: &$crate::NeutralType) -> types::Logicaltype {
                match t {
                    $crate::NeutralType::Boolean => types::Logicaltype::Boolean,
                    $crate::NeutralType::Int64 => types::Logicaltype::Int64,
                    $crate::NeutralType::Float64 => types::Logicaltype::Float64,
                    $crate::NeutralType::Text => types::Logicaltype::Text,
                    $crate::NeutralType::Blob => types::Logicaltype::Blob,
                    $crate::NeutralType::Complex(e) => {
                        types::Logicaltype::Complex(e.clone().into())
                    }
                }
            }

            fn duckerr(e: ::std::string::String) -> types::Duckerror {
                types::Duckerror::Invalidargument(e)
            }

            // ---- guest::Guest (load / reconfigure / shutdown) ----

            impl guest::Guest for Extension {
                fn load() -> Result<types::Loadresult, types::Duckerror> {
                    register_functions()?;
                    Ok(types::Loadresult {
                        name: <Core as $crate::ExtCore>::NAME.into(),
                        version: Some(<Core as $crate::ExtCore>::VERSION.into()),
                        requires: ::std::vec::Vec::new().into(),
                    })
                }
                fn reconfigure(
                    _keys: ::std::vec::Vec<::std::string::String>,
                ) -> Result<bool, types::Duckerror> {
                    Ok(false)
                }
                fn shutdown() -> Result<bool, types::Duckerror> {
                    Ok(false)
                }
            }

            // ---- Handle table (u32 -> DECLS index), shared by the
            //      scalar and aggregate registries. ----

            fn handle_table(
            ) -> &'static ::std::sync::Mutex<::std::collections::HashMap<u32, usize>> {
                static T: ::std::sync::OnceLock<
                    ::std::sync::Mutex<::std::collections::HashMap<u32, usize>>,
                > = ::std::sync::OnceLock::new();
                T.get_or_init(|| ::std::sync::Mutex::new(::std::collections::HashMap::new()))
            }
            static NEXT_HANDLE: ::std::sync::atomic::AtomicU32 =
                ::std::sync::atomic::AtomicU32::new(1);

            fn idx_for(handle: u32) -> Result<usize, types::Duckerror> {
                handle_table()
                    .lock()
                    .expect("handle mutex poisoned")
                    .get(&handle)
                    .copied()
                    .ok_or_else(|| types::Duckerror::Internal("unknown handle".into()))
            }

            // ---- callback_dispatch::Guest ----

            impl callback_dispatch::Guest for Extension {
                fn call_scalar_batch(
                    handle: u32,
                    rows: ::std::vec::Vec<::std::vec::Vec<types::Duckvalue>>,
                    ctx: types::Invokeinfo,
                ) -> Result<::std::vec::Vec<types::Duckvalue>, types::Duckerror> {
                    let base = ctx.rowindex.unwrap_or(0);
                    let mut out = ::std::vec::Vec::with_capacity(rows.len());
                    for (i, args) in rows.into_iter().enumerate() {
                        let row_ctx = types::Invokeinfo {
                            rowindex: Some(base + i as u64),
                            iswindow: ctx.iswindow,
                        };
                        out.push(Self::call_scalar(handle, args, row_ctx)?);
                    }
                    Ok(out)
                }

                fn call_scalar(
                    handle: u32,
                    args: ::std::vec::Vec<types::Duckvalue>,
                    _ctx: types::Invokeinfo,
                ) -> Result<types::Duckvalue, types::Duckerror> {
                    let idx = idx_for(handle)?;
                    let decl = &<Core as $crate::ExtCore>::DECLS[idx];
                    let neutral: ::std::vec::Vec<$crate::NeutralValue> =
                        args.iter().map(to_neutral).collect();
                    if matches!(decl.null_handling, $crate::NullHandling::Propagate)
                        && neutral.iter().any(|v| v.is_null())
                    {
                        return Ok(types::Duckvalue::Null);
                    }
                    let res = <Core as $crate::ExtCore>::dispatch(idx, &neutral)
                        .map_err(duckerr)?;
                    Ok(from_neutral(res))
                }

                fn call_table(
                    _handle: u32,
                    _args: ::std::vec::Vec<types::Duckvalue>,
                ) -> Result<types::Resultset, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no table functions", <Core as $crate::ExtCore>::NAME),
                    ))
                }

                fn call_aggregate(
                    handle: u32,
                    rows: types::Rowbatch,
                ) -> Result<types::Duckvalue, types::Duckerror> {
                    let idx = idx_for(handle)?;
                    // Convert the whole buffered group to neutral rows once,
                    // then fold in-guest via the core (state never marshals).
                    let neutral_rows: ::std::vec::Vec<::std::vec::Vec<$crate::NeutralValue>> =
                        rows.iter()
                            .map(|row| row.iter().map(to_neutral).collect())
                            .collect();
                    let row_refs: ::std::vec::Vec<&[$crate::NeutralValue]> =
                        neutral_rows.iter().map(|r| r.as_slice()).collect();
                    let res = <Core as $crate::ExtCore>::dispatch_aggregate(idx, &row_refs)
                        .map_err(duckerr)?;
                    Ok(from_neutral(res))
                }

                fn call_pragma(
                    _handle: u32,
                    _args: ::std::vec::Vec<types::Duckvalue>,
                ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no pragmas", <Core as $crate::ExtCore>::NAME),
                    ))
                }
                fn call_cast(
                    _handle: u32,
                    _value: types::Duckvalue,
                ) -> Result<types::Duckvalue, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no casts", <Core as $crate::ExtCore>::NAME),
                    ))
                }
            }

            $export!(Extension);

            // ---- Registration: scalars via the scalar registry,
            //      aggregates via the aggregate registry. ----

            fn register_functions() -> Result<(), types::Duckerror> {
                let mut scalar_registry: Option<runtime::ScalarRegistry> = None;
                let mut agg_registry: Option<runtime::AggregateRegistry> = None;

                for (idx, decl) in <Core as $crate::ExtCore>::DECLS.iter().enumerate() {
                    let handle =
                        NEXT_HANDLE.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
                    handle_table()
                        .lock()
                        .expect("handle mutex poisoned")
                        .insert(handle, idx);
                    let args: ::std::vec::Vec<runtime::Funcarg> = decl
                        .args
                        .iter()
                        .map(|t| runtime::Funcarg {
                            name: Some("value".into()),
                            logical: ntype_to_logical(t),
                        })
                        .collect();
                    let mut attributes = types::Funcflags::STATELESS;
                    if decl.deterministic {
                        attributes |= types::Funcflags::DETERMINISTIC;
                    }
                    let opts = runtime::Funcopts {
                        description: Some(
                            ::std::format!("{} {}", <Core as $crate::ExtCore>::NAME,
                                match decl.kind {
                                    $crate::CapabilityKind::Aggregate => "aggregate",
                                    $crate::CapabilityKind::Scalar => "scalar",
                                }),
                        ),
                        tags: ::std::vec![<Core as $crate::ExtCore>::NAME.into()],
                        attributes,
                    };
                    match decl.kind {
                        $crate::CapabilityKind::Scalar => {
                            let registry = match scalar_registry {
                                Some(ref r) => r,
                                None => {
                                    let cap = runtime::get_capability(
                                        types::Capabilitykind::Scalar,
                                    )
                                    .ok_or_else(|| {
                                        types::Duckerror::Internal(
                                            "host did not expose scalar capability".into(),
                                        )
                                    })?;
                                    let r = match cap {
                                        runtime::Capability::Scalar(r) => r,
                                        _ => {
                                            return Err(types::Duckerror::Internal(
                                                "scalar capability returned unexpected variant"
                                                    .into(),
                                            ))
                                        }
                                    };
                                    scalar_registry = Some(r);
                                    scalar_registry.as_ref().unwrap()
                                }
                            };
                            registry.register(
                                decl.name,
                                &args,
                                &ntype_to_logical(&decl.ret),
                                runtime::ScalarCallback::new(handle),
                                Some(&opts),
                            )?;
                        }
                        $crate::CapabilityKind::Aggregate => {
                            let registry = match agg_registry {
                                Some(ref r) => r,
                                None => {
                                    let cap = runtime::get_capability(
                                        types::Capabilitykind::Aggregate,
                                    )
                                    .ok_or_else(|| {
                                        types::Duckerror::Internal(
                                            "host did not expose aggregate capability".into(),
                                        )
                                    })?;
                                    let r = match cap {
                                        runtime::Capability::Aggregate(r) => r,
                                        _ => {
                                            return Err(types::Duckerror::Internal(
                                                "aggregate capability returned unexpected variant"
                                                    .into(),
                                            ))
                                        }
                                    };
                                    agg_registry = Some(r);
                                    agg_registry.as_ref().unwrap()
                                }
                            };
                            registry.register(
                                decl.name,
                                &args,
                                &ntype_to_logical(&decl.ret),
                                runtime::AggregateCallback::new(handle),
                                Some(&opts),
                            )?;
                        }
                    }
                }
                Ok(())
            }
        };
    };
}
