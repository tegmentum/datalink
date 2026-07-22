//! `duckdb_shim!`: generate the ducklink (`duckdb:extension`) shim from
//! an [`ExtCore`](crate::ExtCore).
//!
//! This is a faithful TRANSCRIPTION of the hand-written glue in
//! ducklink `extensions/aba-component/src/lib.rs` — the
//! `guest::Guest` (load/reconfigure/shutdown), the
//! `callback_dispatch::Guest` (the six call_* arms), the `u32` handle
//! table, `register_functions()`, and the `Duckvalue` marshalling —
//! generalized over `Core::DECLS`. Only names/arg-ret-types/bodies
//! varied across extensions; those all come from the declaration now.
//!
//! # Parameterization
//!
//! The consuming crate runs its own `wit_bindgen::generate!` (ducklink:
//! `duckdb:extension@2.2.0`+, wit-bindgen 0.41) and passes the resulting
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
//!
//! # Table-valued functions (T4)
//!
//! When [`FnDecl::kind`](crate::FnDecl) is
//! [`CapabilityKind::Table`](crate::CapabilityKind), the generated
//! `register_functions()` reaches for the `table` capability instead of
//! `scalar`, converts [`FnDecl::columns`](crate::FnDecl) to
//! `runtime::Columndef`, and registers a `TableCallback` bound to a
//! sequential `u32` handle. `callback_dispatch::call_table` then looks
//! up handle → DECLS-idx and calls [`ExtCore::dispatch_table`](crate::ExtCore::dispatch_table).
//! If a table decl carries `replacement_scan_extensions`, the OPTIONAL
//! `files = ...;` parameter's `register_replacement_scan` is invoked
//! with the `u32` registration-id `table-registry.register` returned.

/// Expand the full ducklink shim for `$core`.
///
/// ```ignore
/// duckdb_shim! {
///     core = aba_core::Core;
///     types = duckdb::extension::types;
///     column_types = duckdb::extension::column_types;
///     runtime = duckdb::extension::runtime;
///     callback_dispatch = exports::duckdb::extension::callback_dispatch;
///     guest = exports::duckdb::extension::guest;
///     export = export;
///     // OPTIONAL — pass when a table decl uses replacement_scan:
///     // files = duckdb::extension::files;
/// }
/// ```
#[macro_export]
macro_rules! duckdb_shim {
    (
        core = $core:path ;
        types = $types:path ;
        column_types = $ct:path ;
        runtime = $rt:path ;
        callback_dispatch = $cbd:path ;
        guest = $guest:path ;
        export = $export:ident ;
        $( files = $files:path ; )?
    ) => {
        const _: () = {
            use $crate::ExtCore as _;
            use $types as types;
            use $ct as col;
            use $rt as runtime;
            use $cbd as callback_dispatch;
            use $guest as guest;
            $( use $files as files; )?

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

            // ---- Columnar marshalling: colvec <-> NeutralColVec ----
            //
            // The major-4 hot path. A `colvec` arrives as a typed contiguous
            // column (the core memcpy'd it from `duckdb_vector_get_data`) plus
            // an out-of-band packed validity bitmap (empty => all valid, the
            // exact layout of `NeutralColVec`). Lifting a fixed-width column is a
            // bulk `clone()` of the slice; var-width (text/blob) and the
            // `complex` escape hatch are element-wise (unavoidable). Cores
            // declare only the neutral closed type set, so the physical-int /
            // temporal arms are widened defensively (never lossy in practice).

            fn colvec_to_neutral(c: &col::Colvec) -> $crate::NeutralColVec {
                let data = match &c.data {
                    col::Column::Boolean(v) => $crate::NeutralColumn::Boolean(v.clone()),
                    col::Column::Int64(v) => $crate::NeutralColumn::Int64(v.clone()),
                    col::Column::Float64(v) => $crate::NeutralColumn::Float64(v.clone()),
                    col::Column::Text(v) => $crate::NeutralColumn::Text(
                        v.iter().map(|s| ::std::string::String::from(s.as_str())).collect(),
                    ),
                    col::Column::Blob(v) => $crate::NeutralColumn::Blob(v.clone()),
                    col::Column::Complex(v) => {
                        let type_expr = v
                            .first()
                            .map(|e| ::std::string::String::from(e.type_expr.as_str()))
                            .unwrap_or_default();
                        let json = v
                            .iter()
                            .map(|e| ::std::string::String::from(e.json.as_str()))
                            .collect();
                        $crate::NeutralColumn::Complex { type_expr, json }
                    }
                    // physical-int / temporal arms widen to Int64 (defensive).
                    col::Column::Int32(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Int16(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Int8(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Uint64(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Uint32(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Uint16(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Uint8(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Date(v) => {
                        $crate::NeutralColumn::Int64(v.iter().map(|&x| x as i64).collect())
                    }
                    col::Column::Time(v) => $crate::NeutralColumn::Int64(v.clone()),
                    col::Column::Timestamp(v) => $crate::NeutralColumn::Int64(v.clone()),
                    col::Column::Timestamptz(v) => $crate::NeutralColumn::Int64(v.clone()),
                    col::Column::Float32(v) => {
                        $crate::NeutralColumn::Float64(v.iter().map(|&x| x as f64).collect())
                    }
                    // composite physical types ride the escape hatch.
                    col::Column::Decimal(v) => $crate::NeutralColumn::Complex {
                        type_expr: "DECIMAL".into(),
                        json: v.iter().map(|d| ::std::format!("{:?}", d)).collect(),
                    },
                    col::Column::Interval(v) => $crate::NeutralColumn::Complex {
                        type_expr: "INTERVAL".into(),
                        json: v.iter().map(|d| ::std::format!("{:?}", d)).collect(),
                    },
                    col::Column::Uuid(v) => $crate::NeutralColumn::Complex {
                        type_expr: "UUID".into(),
                        json: v.iter().map(|d| ::std::format!("{:?}", d)).collect(),
                    },
                };
                $crate::NeutralColVec {
                    data,
                    validity: c.validity.clone(),
                    rows: c.rows as usize,
                }
            }

            fn neutral_to_colvec(n: $crate::NeutralColVec) -> col::Colvec {
                let rows = n.rows as u32;
                let validity = n.validity;
                let data = match n.data {
                    $crate::NeutralColumn::Boolean(v) => col::Column::Boolean(v),
                    $crate::NeutralColumn::Int64(v) => col::Column::Int64(v),
                    $crate::NeutralColumn::Float64(v) => col::Column::Float64(v),
                    $crate::NeutralColumn::Text(v) => {
                        col::Column::Text(v.into_iter().map(|s| s.into()).collect())
                    }
                    $crate::NeutralColumn::Blob(v) => col::Column::Blob(v),
                    $crate::NeutralColumn::Complex { type_expr, json } => col::Column::Complex(
                        json.into_iter()
                            .map(|j| col::Complexvalue {
                                type_expr: type_expr.clone().into(),
                                json: j.into(),
                            })
                            .collect(),
                    ),
                };
                col::Colvec { data, validity, rows }
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

            // ---- Handle table (u32 -> DECLS index) ----
            //
            // Shared by scalar and table registrations: the host guarantees
            // it re-hands the same handle to the appropriate call_* arm, so
            // one map is safe for both kinds.

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
                // major-4 HOT PATH: one columnar call per DataChunk. The args
                // arrive as typed `colvec`s (bulk-memcpy'd by the core from the
                // DuckDB vectors); `scalar_batch_col` bridges to the existing
                // per-row neutral `dispatch` so the core needs ZERO changes,
                // applies NULL propagation, and accumulates one typed output
                // column. Semantically identical to the major-3 per-row loop.
                fn call_scalar_batch_col(
                    handle: u32,
                    args: ::std::vec::Vec<callback_dispatch::Colvec>,
                    _ctx: types::Invokeinfo,
                ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
                    let idx = handle_table()
                        .lock()
                        .expect("scalar handle mutex poisoned")
                        .get(&handle)
                        .copied()
                        .ok_or_else(|| {
                            types::Duckerror::Internal("unknown scalar handle".into())
                        })?;
                    let decl = &<Core as $crate::ExtCore>::DECLS[idx];
                    let propagate =
                        matches!(decl.null_handling, $crate::NullHandling::Propagate);
                    let neutral_args: ::std::vec::Vec<$crate::NeutralColVec> =
                        args.iter().map(colvec_to_neutral).collect();
                    let out = $crate::scalar_batch_col(
                        idx,
                        propagate,
                        &decl.ret,
                        &neutral_args,
                        <Core as $crate::ExtCore>::dispatch,
                    )
                    .map_err(duckerr)?;
                    Ok(neutral_to_colvec(out))
                }

                fn call_aggregate_col(
                    _handle: u32,
                    _args: ::std::vec::Vec<callback_dispatch::Colvec>,
                ) -> Result<types::Duckvalue, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no aggregates", <Core as $crate::ExtCore>::NAME),
                    ))
                }

                fn call_cast_col(
                    _handle: u32,
                    _arg: callback_dispatch::Colvec,
                ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
                    Err(types::Duckerror::Unsupported(
                        ::std::format!("{}: no casts", <Core as $crate::ExtCore>::NAME),
                    ))
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

                // major-4 TABLE PATH: the host passes one arg list (the TVF's
                // INPUT is a single call site, its OUTPUT is many rows). We
                // resolve handle -> DECLS-idx, run the core's `dispatch_table`,
                // then marshal each `Vec<NeutralValue>` row into a `Vec<Duckvalue>`.
                fn call_table(
                    handle: u32,
                    args: ::std::vec::Vec<types::Duckvalue>,
                ) -> Result<types::Resultset, types::Duckerror> {
                    let idx = handle_table()
                        .lock()
                        .expect("table handle mutex poisoned")
                        .get(&handle)
                        .copied()
                        .ok_or_else(|| {
                            types::Duckerror::Internal(::std::format!(
                                "{}: unknown table handle {}",
                                <Core as $crate::ExtCore>::NAME,
                                handle
                            ))
                        })?;
                    let neutral: ::std::vec::Vec<$crate::NeutralValue> =
                        args.iter().map(to_neutral).collect();
                    let rows = <Core as $crate::ExtCore>::dispatch_table(idx, &neutral)
                        .map_err(duckerr)?;
                    let out: types::Resultset = rows
                        .into_iter()
                        .map(|row| row.into_iter().map(from_neutral).collect())
                        .collect();
                    Ok(out)
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

            // ---- Registration ----
            //
            // Iterate DECLS; register scalars via the scalar registry (as
            // before) and tables via the table registry. Aggregate decls
            // shouldn't appear on this shim (use `duckdb_agg_shim!`); they
            // fall through untouched here (a defensive no-op).
            //
            // Each decl gets its own `u32` handle; the returned u32 from
            // `table-registry.register` is the `table-function-handle` used
            // by the `files` replacement-scan hook.

            fn register_functions() -> Result<(), types::Duckerror> {
                let mut scalar_registry: ::std::option::Option<runtime::ScalarRegistry> = None;
                let mut table_registry: ::std::option::Option<runtime::TableRegistry> = None;

                for (idx, decl) in
                    <Core as $crate::ExtCore>::DECLS.iter().enumerate()
                {
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
                    match decl.kind {
                        $crate::CapabilityKind::Scalar => {
                            let registry = match scalar_registry.as_ref() {
                                Some(r) => r,
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
                            let mut attributes = types::Funcflags::STATELESS;
                            if decl.deterministic {
                                attributes |= types::Funcflags::DETERMINISTIC;
                            }
                            let opts = runtime::Funcopts {
                                description: Some(::std::format!(
                                    "{} scalar",
                                    <Core as $crate::ExtCore>::NAME
                                )),
                                tags: ::std::vec![
                                    <Core as $crate::ExtCore>::NAME.into()
                                ],
                                attributes,
                            };
                            registry.register(
                                decl.name,
                                &args,
                                &ntype_to_logical(&decl.ret),
                                runtime::ScalarCallback::new(handle),
                                Some(&opts),
                            )?;
                        }
                        $crate::CapabilityKind::Table => {
                            let registry = match table_registry.as_ref() {
                                Some(r) => r,
                                None => {
                                    let cap = runtime::get_capability(
                                        types::Capabilitykind::Table,
                                    )
                                    .ok_or_else(|| {
                                        types::Duckerror::Internal(
                                            "host did not expose table capability".into(),
                                        )
                                    })?;
                                    let r = match cap {
                                        runtime::Capability::Table(r) => r,
                                        _ => {
                                            return Err(types::Duckerror::Internal(
                                                "table capability returned unexpected variant"
                                                    .into(),
                                            ))
                                        }
                                    };
                                    table_registry = Some(r);
                                    table_registry.as_ref().unwrap()
                                }
                            };
                            let columns: ::std::vec::Vec<runtime::Columndef> = decl
                                .columns
                                .iter()
                                .map(|c| runtime::Columndef {
                                    name: c.name.into(),
                                    logical: ntype_to_logical(&c.ntype),
                                })
                                .collect();
                            let opts = runtime::Extopts {
                                description: Some(::std::format!(
                                    "{} table function",
                                    <Core as $crate::ExtCore>::NAME
                                )),
                                tags: ::std::vec![
                                    <Core as $crate::ExtCore>::NAME.into()
                                ],
                            };
                            let _reg_id = registry.register(
                                decl.name,
                                &args,
                                &columns,
                                runtime::TableCallback::new(handle),
                                Some(&opts),
                            )?;
                            // Replacement-scan hook: only fires when the
                            // decl declares extensions AND the shim was
                            // invoked with `files = ...;`. The generated
                            // code that references `files::` is folded
                            // inside a `$(...)?` guard so the shim still
                            // compiles when `files=` was omitted.
                            $(
                                {
                                    // Bring `$files` into scope for this block
                                    // (the outer `use $files as files;` is also
                                    // guarded, so we do not depend on it here).
                                    use $files as _files_alias;
                                    if !decl.replacement_scan_extensions.is_empty() {
                                        let exts: ::std::vec::Vec<::std::string::String> =
                                            decl.replacement_scan_extensions
                                                .iter()
                                                .map(|s| (*s).into())
                                                .collect();
                                        let scan = _files_alias::ReplacementScan {
                                            extensions: exts,
                                            table_function: _reg_id,
                                            mode: _files_alias::DetectionMode::ExtensionOnly,
                                        };
                                        // A failed replacement-scan hook is not
                                        // fatal to the load — surface as
                                        // Duckerror::Internal to match style.
                                        _files_alias::register_replacement_scan(&scan)
                                            .map_err(|e| types::Duckerror::Internal(
                                                ::std::format!(
                                                    "{}: register_replacement_scan failed: {}",
                                                    <Core as $crate::ExtCore>::NAME,
                                                    e
                                                )
                                            ))?;
                                    }
                                }
                            )?
                        }
                        $crate::CapabilityKind::Aggregate => {
                            // Aggregates require `duckdb_agg_shim!`; this
                            // shim silently skips them so a mixed core that
                            // accidentally uses the wrong shim still loads
                            // its scalars/tables (aggregates just won't be
                            // callable — a runtime discovery failure).
                        }
                    }
                }
                Ok(())
            }
        };
    };
}
