//! Columnar bridge for HAND-WRITTEN `duckdb:extension` components.
//!
//! The major-4 contract made the scalar/aggregate/cast HOT PATH columnar:
//! `callback-dispatch` exports `call-scalar-batch-col` / `call-aggregate-col`
//! / `call-cast-col` (typed `colvec`s, one bulk memcpy per fixed-width column)
//! and REMOVED the major-3 row-major `call-scalar-batch` / `call-aggregate`.
//!
//! Components whose logic is pulled up onto a [`crate::declare!`] core get the
//! columnar dispatch for free from [`duckdb_shim!`](crate::duckdb_shim). But the
//! HEAVY / SPECIAL components (C-lib deps, network, storage, index, parsers,
//! table generators) keep their existing HAND-WRITTEN per-row logic; there is no
//! logic to pull up. For those this module supplies the SAME `colvec` <-> row
//! adapter the codegen uses, as drop-in macros, so the migration is mechanical
//! and introduces ZERO logic change.
//!
//!   * [`columnar_bridge!`] — for a component that has SCALAR (and optionally
//!     CAST) functions. Generates the WHOLE `impl callback_dispatch::Guest`:
//!     the columnar hot methods bridge `colvec` <-> row and delegate to the
//!     user's per-row `scalar` (and `cast`) free fns; the cold singleton
//!     `call-scalar` / `call-cast` delegate to the same fns; `call-table` /
//!     `call-pragma` / `call-aggregate-col` are `Unsupported`.
//!   * [`columnar_stub!`] — for a component with NO scalar/cast (table /
//!     storage / index / parser / network table fns). Generates only the three
//!     columnar methods as `Unsupported`; the component writes the rest of the
//!     impl (its `call-table` etc.) by hand.
//!
//! NULL is carried out-of-band in `colvec.validity` (packed LE bitmap; empty =
//! all-valid), byte-for-byte DuckDB's mask. The output column type is inferred
//! from the first non-null result (a scalar's return type is fixed). The
//! hand-written `call-scalar-batch-col` ABI does NOT carry the declared return
//! type, so an all-null result column (no value to infer from) falls back to a
//! `text` placeholder; this is harmless because the @4.0.0 core writes an
//! all-null result column using the function's DECLARED return type and ignores
//! the placeholder's variant (see `write_colvec_to_vector` / `colvec_all_null`
//! in duckdb-wasm core). So a declared-`blob` (or any var-width) function whose
//! result is all-null returns correct typed NULLs regardless of this fallback.
//! (The codegen `duckdb_shim!` path has the declared `ret` in hand and builds
//! the correctly-typed all-null column directly via `scalar_batch_col`.)

/// Generate the full `impl callback_dispatch::Guest` for a hand-written
/// component with scalar (and optionally cast) functions.
///
/// ```ignore
/// columnar_bridge! {
///     types = duckdb::extension::types;
///     column_types = duckdb::extension::column_types;
///     callback_dispatch = exports::duckdb::extension::callback_dispatch;
///     target = Extension;
///     scalar = my_scalar;            // fn(u32, Vec<Duckvalue>, Invokeinfo) -> Result<Duckvalue, Duckerror>
///     // optional:  cast = my_cast;  // fn(u32, Duckvalue) -> Result<Duckvalue, Duckerror>
/// }
/// ```
#[macro_export]
macro_rules! columnar_bridge {
    (
        types = $t:path ;
        column_types = $ct:path ;
        callback_dispatch = $cb:path ;
        target = $target:ty ;
        scalar = $scalar:path ;
        $( cast = $cast:path ; )?
        $( scalar_batch_col = $sbc:path ; )?
    ) => {
        const _: () = {
            use $t as types;
            use $ct as col;
            use $cb as callback_dispatch;

            $crate::__columnar_bridge_conv!(types, col);

            #[allow(clippy::all, unreachable_code)]
            fn __bridge_cast(h: u32, v: types::Duckvalue)
                -> ::std::result::Result<types::Duckvalue, types::Duckerror>
            {
                $( return $cast(h, v); )?
                let _ = (h, v);
                ::std::result::Result::Err(types::Duckerror::Unsupported(
                    "no casts (bridge)".into(),
                ))
            }

            // The columnar hot path. By default this is the generic
            // `colvec` -> row -> per-row-`scalar` -> `colvec` adapter (zero
            // logic change for a migrated component). A component that supplies
            // `scalar_batch_col = <fn>;` gets a TRUE column-at-a-time kernel
            // instead: the override reads the typed `colvec`s directly (no
            // per-row `Vec<Vec<duckvalue>>` materialization, no per-cell
            // `duckvalue` boxing/string clone) and builds the output column
            // directly. The override is byte-identical to the per-row `scalar`;
            // it only removes the marshalling that the columnar ABI made
            // unnecessary. This is where the compute-heavy components claw back
            // the row-materialization overhead the bridge otherwise pays.
            #[allow(clippy::all, unreachable_code, unused_variables)]
            fn __bridge_scalar_batch_col(
                handle: u32,
                args: &[callback_dispatch::Colvec],
                ctx: types::Invokeinfo,
            ) -> ::std::result::Result<callback_dispatch::Colvec, types::Duckerror> {
                $( return $sbc(handle, args, ctx); )?
                let base = ctx.rowindex.unwrap_or(0);
                let rows = __bridge_colvecs_to_rows(args);
                let mut out = ::std::vec::Vec::with_capacity(rows.len());
                for (i, a) in rows.into_iter().enumerate() {
                    let row_ctx = types::Invokeinfo {
                        rowindex: ::std::option::Option::Some(base + i as u64),
                        iswindow: ctx.iswindow,
                    };
                    out.push($scalar(handle, a, row_ctx)?);
                }
                ::std::result::Result::Ok(__bridge_vals_to_colvec(out))
            }

            impl callback_dispatch::Guest for $target {
                fn call_scalar_batch_col(
                    handle: u32,
                    args: ::std::vec::Vec<callback_dispatch::Colvec>,
                    ctx: types::Invokeinfo,
                ) -> ::std::result::Result<callback_dispatch::Colvec, types::Duckerror> {
                    __bridge_scalar_batch_col(handle, &args, ctx)
                }

                fn call_aggregate_col(
                    _handle: u32,
                    _args: ::std::vec::Vec<callback_dispatch::Colvec>,
                ) -> ::std::result::Result<types::Duckvalue, types::Duckerror> {
                    ::std::result::Result::Err(types::Duckerror::Unsupported(
                        "no columnar aggregate (bridge)".into(),
                    ))
                }

                fn call_cast_col(
                    handle: u32,
                    arg: callback_dispatch::Colvec,
                ) -> ::std::result::Result<callback_dispatch::Colvec, types::Duckerror> {
                    let rows = __bridge_colvecs_to_rows(::std::slice::from_ref(&arg));
                    let mut out = ::std::vec::Vec::with_capacity(rows.len());
                    for a in rows {
                        let v = a.into_iter().next().unwrap_or(types::Duckvalue::Null);
                        out.push(__bridge_cast(handle, v)?);
                    }
                    ::std::result::Result::Ok(__bridge_vals_to_colvec(out))
                }

                fn call_scalar(
                    handle: u32,
                    args: ::std::vec::Vec<types::Duckvalue>,
                    ctx: types::Invokeinfo,
                ) -> ::std::result::Result<types::Duckvalue, types::Duckerror> {
                    $scalar(handle, args, ctx)
                }

                fn call_table(
                    _handle: u32,
                    _args: ::std::vec::Vec<types::Duckvalue>,
                ) -> ::std::result::Result<types::Resultset, types::Duckerror> {
                    ::std::result::Result::Err(types::Duckerror::Unsupported(
                        "no table functions (bridge)".into(),
                    ))
                }

                fn call_pragma(
                    _handle: u32,
                    _args: ::std::vec::Vec<types::Duckvalue>,
                ) -> ::std::result::Result<::std::option::Option<types::Duckvalue>, types::Duckerror>
                {
                    ::std::result::Result::Err(types::Duckerror::Unsupported(
                        "no pragmas (bridge)".into(),
                    ))
                }

                fn call_cast(
                    handle: u32,
                    value: types::Duckvalue,
                ) -> ::std::result::Result<types::Duckvalue, types::Duckerror> {
                    __bridge_cast(handle, value)
                }
            }
        };
    };
}

/// Generate ONLY the three columnar methods as `Unsupported`, for a component
/// that has no scalar/cast (table / storage / index / parser). Place inside the
/// component's `impl callback_dispatch::Guest`; write the cold methods by hand.
///
/// ```ignore
/// impl callback_dispatch::Guest for Extension {
///     columnar_stub! {
///         types = duckdb::extension::types;
///         callback_dispatch = exports::duckdb::extension::callback_dispatch;
///     }
///     fn call_table(...) { /* the real table fn, unchanged */ }
///     fn call_scalar(...) { ... }
///     fn call_pragma(...) { ... }
///     fn call_cast(...) { ... }
/// }
/// ```
///
/// Assumes the component's impl module has the conventional imports in scope:
/// `use exports::duckdb::extension::callback_dispatch;` and
/// `use duckdb::extension::types;`.
#[macro_export]
macro_rules! columnar_stub {
    () => {
        fn call_scalar_batch_col(
            _handle: u32,
            _args: ::std::vec::Vec<callback_dispatch::Colvec>,
            _ctx: types::Invokeinfo,
        ) -> ::std::result::Result<callback_dispatch::Colvec, types::Duckerror> {
            ::std::result::Result::Err(types::Duckerror::Unsupported(
                "no scalar functions (bridge stub)".into(),
            ))
        }
        fn call_aggregate_col(
            _handle: u32,
            _args: ::std::vec::Vec<callback_dispatch::Colvec>,
        ) -> ::std::result::Result<types::Duckvalue, types::Duckerror> {
            ::std::result::Result::Err(types::Duckerror::Unsupported(
                "no aggregate (bridge stub)".into(),
            ))
        }
        fn call_cast_col(
            _handle: u32,
            _arg: callback_dispatch::Colvec,
        ) -> ::std::result::Result<callback_dispatch::Colvec, types::Duckerror> {
            ::std::result::Result::Err(types::Duckerror::Unsupported(
                "no cast (bridge stub)".into(),
            ))
        }
    };
}

/// Internal: emit the two `colvec` <-> row conversion fns. The caller has
/// already aliased the binding modules to the idents `$types` and `$col`.
#[doc(hidden)]
#[macro_export]
macro_rules! __columnar_bridge_conv {
    ($types:ident, $col:ident) => {
        #[allow(dead_code, clippy::all)]
        fn __bridge_col_to_vals(c: &$col::Colvec) -> ::std::vec::Vec<$types::Duckvalue> {
            use $types::Duckvalue as V;
            let n = c.rows as usize;
            let valid = |i: usize| -> bool {
                if c.validity.is_empty() {
                    return true;
                }
                c.validity
                    .get(i / 8)
                    .map(|b| (b >> (i % 8)) & 1 == 1)
                    .unwrap_or(false)
            };
            let mut out = ::std::vec::Vec::with_capacity(n);
            macro_rules! lift {
                ($v:expr, $arm:path) => {{
                    let data = $v;
                    for i in 0..n {
                        out.push(if valid(i) { $arm(data[i].clone()) } else { V::Null });
                    }
                }};
            }
            match &c.data {
                $col::Column::Boolean(v) => lift!(v, V::Boolean),
                $col::Column::Int64(v) => lift!(v, V::Int64),
                $col::Column::Uint64(v) => lift!(v, V::Uint64),
                $col::Column::Float64(v) => lift!(v, V::Float64),
                $col::Column::Int32(v) => lift!(v, V::Int32),
                $col::Column::Timestamp(v) => lift!(v, V::Timestamp),
                $col::Column::Int8(v) => lift!(v, V::Int8),
                $col::Column::Int16(v) => lift!(v, V::Int16),
                $col::Column::Uint8(v) => lift!(v, V::Uint8),
                $col::Column::Uint16(v) => lift!(v, V::Uint16),
                $col::Column::Uint32(v) => lift!(v, V::Uint32),
                $col::Column::Float32(v) => lift!(v, V::Float32),
                $col::Column::Date(v) => lift!(v, V::Date),
                $col::Column::Time(v) => lift!(v, V::Time),
                $col::Column::Timestamptz(v) => lift!(v, V::Timestamptz),
                $col::Column::Text(v) => {
                    for i in 0..n {
                        out.push(if valid(i) { V::Text(v[i].clone()) } else { V::Null });
                    }
                }
                $col::Column::Blob(v) => {
                    for i in 0..n {
                        out.push(if valid(i) { V::Blob(v[i].clone()) } else { V::Null });
                    }
                }
                $col::Column::Decimal(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Decimal($types::Decimalvalue {
                                lower: v[i].lower,
                                upper: v[i].upper,
                                width: v[i].width,
                                scale: v[i].scale,
                            })
                        } else {
                            V::Null
                        });
                    }
                }
                $col::Column::Interval(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Interval($types::Intervalvalue {
                                months: v[i].months,
                                days: v[i].days,
                                micros: v[i].micros,
                            })
                        } else {
                            V::Null
                        });
                    }
                }
                $col::Column::Uuid(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Uuid($types::Uuidvalue { hi: v[i].hi, lo: v[i].lo })
                        } else {
                            V::Null
                        });
                    }
                }
                $col::Column::Complex(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Complex($types::Complexvalue {
                                type_expr: v[i].type_expr.clone(),
                                json: v[i].json.clone(),
                            })
                        } else {
                            V::Null
                        });
                    }
                }
                // major-5: HUGEINT / UHUGEINT 128-bit integer columns.
                $col::Column::Hugeint(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Hugeint($types::Hugeintvalue {
                                lower: v[i].lower,
                                upper: v[i].upper,
                            })
                        } else {
                            V::Null
                        });
                    }
                }
                $col::Column::Uhugeint(v) => {
                    for i in 0..n {
                        out.push(if valid(i) {
                            V::Uhugeint($types::Uhugeintvalue {
                                lower: v[i].lower,
                                upper: v[i].upper,
                            })
                        } else {
                            V::Null
                        });
                    }
                }
                // major-5 S1 nested-column arms carry opaque byte payloads
                // that this row-per-cell bridge cannot faithfully round-trip
                // through per-row `duckvalue`. Components on this bridge do
                // not currently register LIST/STRUCT/MAP/ARRAY scalars, so we
                // surface each row as NULL (matching the all-null fallback
                // path); the columnar override (`scalar_batch_col`) is where a
                // nested-aware component would opt out of the bridge.
                $col::Column::ListCol(_)
                | $col::Column::StructCol(_)
                | $col::Column::MapCol(_)
                | $col::Column::ArrayCol(_) => {
                    for _ in 0..n {
                        out.push(V::Null);
                    }
                }
            }
            out
        }

        #[allow(dead_code, clippy::all)]
        fn __bridge_colvecs_to_rows(
            args: &[$col::Colvec],
        ) -> ::std::vec::Vec<::std::vec::Vec<$types::Duckvalue>> {
            let cols: ::std::vec::Vec<::std::vec::Vec<$types::Duckvalue>> =
                args.iter().map(__bridge_col_to_vals).collect();
            let rows = args.first().map(|c| c.rows as usize).unwrap_or(0);
            let mut out = ::std::vec::Vec::with_capacity(rows);
            for i in 0..rows {
                out.push(cols.iter().map(|c| c[i].clone()).collect());
            }
            out
        }

        #[allow(dead_code, clippy::all)]
        fn __bridge_vals_to_colvec(vals: ::std::vec::Vec<$types::Duckvalue>) -> $col::Colvec {
            use $types::Duckvalue as V;
            let rows = vals.len() as u32;
            let mut validity = ::std::vec![0u8; vals.len().div_ceil(8)];
            let mut any_null = false;
            for (i, v) in vals.iter().enumerate() {
                if matches!(v, V::Null) {
                    any_null = true;
                } else {
                    validity[i / 8] |= 1 << (i % 8);
                }
            }
            let validity = if any_null {
                validity
            } else {
                ::std::vec::Vec::new()
            };
            let first = vals.iter().find(|v| !matches!(v, V::Null));
            macro_rules! gather {
                ($arm:path, $def:expr) => {
                    vals.iter()
                        .map(|v| match v {
                            $arm(x) => x.clone(),
                            _ => $def,
                        })
                        .collect()
                };
            }
            let data = match first {
                Some(V::Boolean(_)) => $col::Column::Boolean(gather!(V::Boolean, false)),
                Some(V::Int64(_)) => $col::Column::Int64(gather!(V::Int64, 0)),
                Some(V::Uint64(_)) => $col::Column::Uint64(gather!(V::Uint64, 0)),
                Some(V::Float64(_)) => $col::Column::Float64(gather!(V::Float64, 0.0)),
                Some(V::Int32(_)) => $col::Column::Int32(gather!(V::Int32, 0)),
                Some(V::Timestamp(_)) => $col::Column::Timestamp(gather!(V::Timestamp, 0)),
                Some(V::Int8(_)) => $col::Column::Int8(gather!(V::Int8, 0)),
                Some(V::Int16(_)) => $col::Column::Int16(gather!(V::Int16, 0)),
                Some(V::Uint8(_)) => $col::Column::Uint8(gather!(V::Uint8, 0)),
                Some(V::Uint16(_)) => $col::Column::Uint16(gather!(V::Uint16, 0)),
                Some(V::Uint32(_)) => $col::Column::Uint32(gather!(V::Uint32, 0)),
                Some(V::Float32(_)) => $col::Column::Float32(gather!(V::Float32, 0.0)),
                Some(V::Date(_)) => $col::Column::Date(gather!(V::Date, 0)),
                Some(V::Time(_)) => $col::Column::Time(gather!(V::Time, 0)),
                Some(V::Timestamptz(_)) => $col::Column::Timestamptz(gather!(V::Timestamptz, 0)),
                Some(V::Text(_)) => $col::Column::Text(
                    vals.iter()
                        .map(|v| match v {
                            V::Text(x) => x.clone(),
                            _ => ::std::string::String::new().into(),
                        })
                        .collect(),
                ),
                Some(V::Blob(_)) => $col::Column::Blob(
                    vals.iter()
                        .map(|v| match v {
                            V::Blob(x) => x.clone(),
                            _ => ::std::vec::Vec::new(),
                        })
                        .collect(),
                ),
                Some(V::Decimal(_)) => $col::Column::Decimal(
                    vals.iter()
                        .map(|v| match v {
                            V::Decimal(x) => $col::Decimalvalue {
                                lower: x.lower,
                                upper: x.upper,
                                width: x.width,
                                scale: x.scale,
                            },
                            _ => $col::Decimalvalue {
                                lower: 0,
                                upper: 0,
                                width: 0,
                                scale: 0,
                            },
                        })
                        .collect(),
                ),
                Some(V::Interval(_)) => $col::Column::Interval(
                    vals.iter()
                        .map(|v| match v {
                            V::Interval(x) => $col::Intervalvalue {
                                months: x.months,
                                days: x.days,
                                micros: x.micros,
                            },
                            _ => $col::Intervalvalue {
                                months: 0,
                                days: 0,
                                micros: 0,
                            },
                        })
                        .collect(),
                ),
                Some(V::Uuid(_)) => $col::Column::Uuid(
                    vals.iter()
                        .map(|v| match v {
                            V::Uuid(x) => $col::Uuidvalue { hi: x.hi, lo: x.lo },
                            _ => $col::Uuidvalue { hi: 0, lo: 0 },
                        })
                        .collect(),
                ),
                Some(V::Complex(_)) => $col::Column::Complex(
                    vals.iter()
                        .map(|v| match v {
                            V::Complex(x) => $col::Complexvalue {
                                type_expr: x.type_expr.clone(),
                                json: x.json.clone(),
                            },
                            _ => $col::Complexvalue {
                                type_expr: ::std::string::String::new().into(),
                                json: ::std::string::String::new().into(),
                            },
                        })
                        .collect(),
                ),
                // major-5: HUGEINT / UHUGEINT 128-bit integer columns.
                Some(V::Hugeint(_)) => $col::Column::Hugeint(
                    vals.iter()
                        .map(|v| match v {
                            V::Hugeint(x) => $col::DuckInt128 {
                                lower: x.lower,
                                upper: x.upper,
                            },
                            _ => $col::DuckInt128 { lower: 0, upper: 0 },
                        })
                        .collect(),
                ),
                Some(V::Uhugeint(_)) => $col::Column::Uhugeint(
                    vals.iter()
                        .map(|v| match v {
                            V::Uhugeint(x) => $col::DuckUint128 {
                                lower: x.lower,
                                upper: x.upper,
                            },
                            _ => $col::DuckUint128 { lower: 0, upper: 0 },
                        })
                        .collect(),
                ),
                // All-null (or empty) result: no value to infer a type from.
                // Emit a `text` placeholder column with an all-invalid validity
                // mask; the @4.0.0 core ignores this variant and writes the
                // all-null result using the DECLARED return type (see the module
                // doc + `colvec_all_null` in the duckdb-wasm core).
                Some(V::Null) | None => $col::Column::Text(
                    vals.iter()
                        .map(|_| ::std::string::String::new().into())
                        .collect(),
                ),
            };
            $col::Colvec {
                data,
                validity,
                rows,
            }
        }
    };
}

