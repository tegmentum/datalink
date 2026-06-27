//! `duckdb_shim!`: generate the ducklink (`duckdb:extension`) shim from
//! an [`ExtCore`](crate::ExtCore).
//!
//! This is a faithful TRANSCRIPTION of the hand-written glue in
//! ducklink `extensions/aba-component/src/lib.rs` — the
//! `guest::Guest` (load/reconfigure/shutdown), the
//! `callback_dispatch::Guest` (the six call_* arms), the `u32` handle
//! table, `register_scalars()`, and the `Duckvalue` marshalling —
//! generalized over `Core::DECLS`. Only names/arg-ret-types/bodies
//! varied across extensions; those all come from the declaration now.
//!
//! # Parameterization
//!
//! The consuming crate runs its own `wit_bindgen::generate!` (ducklink:
//! `duckdb:extension@2.2.0`, wit-bindgen 0.41) and passes the resulting
//! binding paths in. Nothing here hardcodes the package/version, so the
//! same macro serves any repo on the `duckdb:extension` contract family.
//!
//! # NULL handling
//!
//! DuckDB's C scalar API propagates NULL by default (a NULL argument
//! yields a NULL result WITHOUT invoking the function). So a
//! [`NullHandling::Propagate`](crate::NullHandling) function registers
//! normally and never sees a NULL — matching the existing hand-written
//! extensions (e.g. `aba_validate(NULL) -> NULL`). For
//! [`NullHandling::Called`](crate::NullHandling) the generated dispatch
//! passes the `Duckvalue::Null` through as [`NeutralValue::Null`].

/// Expand the full ducklink shim for `$core`.
///
/// ```ignore
/// duckdb_shim! {
///     core = aba_core::Core;
///     types = duckdb::extension::types;
///     runtime = duckdb::extension::runtime;
///     callback_dispatch = exports::duckdb::extension::callback_dispatch;
///     guest = exports::duckdb::extension::guest;
///     export = export;            // the generated export! macro
/// }
/// ```
#[macro_export]
macro_rules! duckdb_shim {
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
            // Closed FROZEN set + the complex() escape hatch ONLY. No new
            // arm is ever introduced here.

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
                    // Any other concrete arm (int32/uint*/date/...) is
                    // outside the neutral closed set; route it through the
                    // escape hatch rather than growing the model.
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
                    register_scalars()?;
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

            // ---- Handle table (u32 -> DECLS index) ----

            fn handle_table(
            ) -> &'static ::std::sync::Mutex<::std::collections::HashMap<u32, usize>> {
                static T: ::std::sync::OnceLock<
                    ::std::sync::Mutex<::std::collections::HashMap<u32, usize>>,
                > = ::std::sync::OnceLock::new();
                T.get_or_init(|| ::std::sync::Mutex::new(::std::collections::HashMap::new()))
            }
            static NEXT_HANDLE: ::std::sync::atomic::AtomicU32 =
                ::std::sync::atomic::AtomicU32::new(1);

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
                    let idx = handle_table()
                        .lock()
                        .expect("scalar handle mutex poisoned")
                        .get(&handle)
                        .copied()
                        .ok_or_else(|| {
                            types::Duckerror::Internal("unknown scalar handle".into())
                        })?;
                    let decl = &<Core as $crate::ExtCore>::DECLS[idx];
                    let neutral: ::std::vec::Vec<$crate::NeutralValue> =
                        args.iter().map(to_neutral).collect();
                    // NULL propagation: DuckDB's default null handling
                    // means a Propagate fn won't be invoked with a NULL
                    // arg, but we honor it defensively for both paths.
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
                    _handle: u32,
                    _rows: types::Rowbatch,
                ) -> Result<types::Duckvalue, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no aggregates", <Core as $crate::ExtCore>::NAME),
                    ))
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

            // ---- Registration (transcribed register_scalars) ----

            fn register_scalars() -> Result<(), types::Duckerror> {
                let capability = runtime::get_capability(types::Capabilitykind::Scalar)
                    .ok_or_else(|| {
                        types::Duckerror::Internal(
                            "host did not expose scalar capability".into(),
                        )
                    })?;
                let registry = match capability {
                    runtime::Capability::Scalar(registry) => registry,
                    _ => {
                        return Err(types::Duckerror::Internal(
                            "scalar capability returned unexpected variant".into(),
                        ))
                    }
                };
                for (idx, decl) in
                    <Core as $crate::ExtCore>::DECLS.iter().enumerate()
                {
                    let handle =
                        NEXT_HANDLE.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
                    handle_table()
                        .lock()
                        .expect("scalar handle mutex poisoned")
                        .insert(handle, idx);
                    let callback = runtime::ScalarCallback::new(handle);
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
                            ::std::format!("{} scalar", <Core as $crate::ExtCore>::NAME),
                        ),
                        tags: ::std::vec![<Core as $crate::ExtCore>::NAME.into()],
                        attributes,
                    };
                    registry.register(
                        decl.name,
                        &args,
                        &ntype_to_logical(&decl.ret),
                        callback,
                        Some(&opts),
                    )?;
                }
                Ok(())
            }
        };
    };
}
