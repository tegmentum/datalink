//! The `declare!` macro: an extension core names its capability table
//! once, and gets the [`ExtCore`](crate::ExtCore) impl (the `DECLS`
//! slice + the neutral `dispatch`) for free. The per-DB shims are then
//! generated from that single declaration.
//!
//! The macro exposes four arms — pick whichever fits your core's
//! capability mix:
//!
//! | arm                              | must have         | may have               |
//! |----------------------------------|-------------------|------------------------|
//! | scalar-only                      | `+` scalar        | -                      |
//! | scalar + aggregate               | `+` aggregate     | `*` scalar             |
//! | pure table (this file, T3)       | `+` table         | -                      |
//! | mixed (scalar/aggregate/table)   | `+` table         | `*` scalar, `*` agg    |
//!
//! Existing scalar-only and scalar+aggregate cores continue to compile
//! byte-for-byte because their arms come first and their `+` repetition
//! anchors them (a `table` decl trailing after scalars/aggregates fails
//! to satisfy either arm and cascades to the table-bearing arms).

/// Map a neutral-type token to a [`NeutralType`](crate::NeutralType).
/// This is the closed FROZEN set plus the `complex("expr")` escape hatch
/// — there is deliberately no way to name a new value arm.
#[macro_export]
#[doc(hidden)]
macro_rules! __ntype {
    (boolean) => { $crate::NeutralType::Boolean };
    (int64)   => { $crate::NeutralType::Int64 };
    (float64) => { $crate::NeutralType::Float64 };
    (text)    => { $crate::NeutralType::Text };
    (blob)    => { $crate::NeutralType::Blob };
    (complex($e:expr)) => { $crate::NeutralType::Complex(::alloc::string::String::from($e)) };
}

/// Map a null-handling token to a [`NullHandling`](crate::NullHandling).
#[macro_export]
#[doc(hidden)]
macro_rules! __nullh {
    (propagate) => { $crate::NullHandling::Propagate };
    (called)    => { $crate::NullHandling::Called };
}

/// Declare an extension core: its name/version and a list of scalar,
/// aggregate, and/or table functions.
///
/// # Scalar-only
///
/// ```ignore
/// datalink_extcore::declare! {
///     core = Core;
///     extension = "aba";
///     version = env!("CARGO_PKG_VERSION");
///
///     scalar aba_validate(text) -> boolean [propagate, deterministic]
///         = |args| Ok(NeutralValue::Boolean(logic::validate(args.arg_text(0, "aba")?)));
/// }
/// ```
///
/// # Scalar + aggregate (aggregate tier)
///
/// See the arm below for the fold-declaration shape.
///
/// # Table (T3, added for the `blast` / `genome-format` pull-up)
///
/// ```ignore
/// datalink_extcore::declare! {
///     core = Core;
///     extension = "range";
///     version = env!("CARGO_PKG_VERSION");
///
///     table range_from_to(int64, int64) -> (v: int64) [deterministic] = |args| {
///         let start = args.arg_int(0, "range_from_to")?;
///         let end = args.arg_int(1, "range_from_to")?;
///         if end < start { return Ok(vec![]); }
///         Ok((start..end).map(|v| vec![NeutralValue::Int64(v)]).collect())
///     };
/// }
/// ```
///
/// A table decl may carry a `replacement_scan` option that wires a file
/// extension (or list of them) to this TVF via the `duckdb:extension`
/// `files` interface:
///
/// ```ignore
/// table read_gbk(text) -> (accession: text, sequence: text)
///     [deterministic, replacement_scan = ["gb", "gbk"]]
///     = |args| { /* ... */ };
/// ```
///
/// # Mixed (scalar + aggregate + table)
///
/// Combine any of `scalar` / `aggregate` / `table`; the mixed arm is
/// picked automatically as soon as at least one `table ...;` is present.
#[macro_export]
macro_rules! declare {
    // ---- Scalar-only form (unchanged) ----
    (
        core = $core:ident;
        extension = $name:expr;
        version = $version:expr;
        $(
            scalar $fname:ident ( $($argt:tt),* ) -> $rett:tt
                [ $nullh:tt , $detkw:ident ]
                = $body:expr ;
        )+
    ) => {
        /// The generated extension core (one per crate). Carries the
        /// capability table + neutral dispatch; both per-DB shims are
        /// derived from this type alone.
        pub struct $core;

        impl $crate::ExtCore for $core {
            const NAME: &'static str = $name;
            const VERSION: &'static str = $version;
            const DECLS: &'static [$crate::FnDecl] = &[
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($fname),
                        kind: $crate::CapabilityKind::Scalar,
                        args: &[ $( $crate::__ntype!($argt) ),* ],
                        ret: $crate::__ntype!($rett),
                        null_handling: $crate::__nullh!($nullh),
                        deterministic: $crate::__declare_det!($detkw),
                        columns: &[],
                        replacement_scan_extensions: &[],
                    }
                ),+
            ];

            fn dispatch(
                idx: usize,
                args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                // One closure per declared function, indexed identically
                // to DECLS. The shim resolves a host handle/func-id to an
                // index, applies null-handling, then calls here.
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let dispatchers: &[fn(&[$crate::NeutralValue])
                    -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                    $( $body ),+
                ];
                match dispatchers.get(idx) {
                    ::core::option::Option::Some(f) => f(args),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: unknown function index {}", $name, idx)
                    ),
                }
            }
        }

        impl $core {
            /// NUL-terminated function names as compile-time literals, in
            /// `DECLS` order. The embed shim needs `&'static [u8]`
            /// NUL-terminated names for `sqlite3_create_function_v2`;
            /// these are derived from the same `$fname` tokens `DECLS`
            /// uses, so they can never drift from the declaration.
            #[allow(dead_code)]
            pub const SCALAR_NAMES_NUL: &'static [&'static [u8]] = &[
                $( ::core::concat!(::core::stringify!($fname), "\0").as_bytes() ),+
            ];
        }
    };

    // ---- Mixed scalar + aggregate form (the aggregate tier) ----
    //
    // Scalars are declared exactly as above; aggregates add a neutral
    // `state` type plus an `init` / `step` / `finalize` fold. DECLS lists
    // all scalars first (kind Scalar), then all aggregates (kind
    // Aggregate); the generated `dispatch_aggregate` maps a DECLS index to
    // an aggregate by subtracting the scalar count. The whole fold runs
    // in one call (the duckdb host buffers a group), so the neutral state
    // is a native Rust value and never marshals across the boundary.
    (
        core = $core:ident;
        extension = $name:expr;
        version = $version:expr;
        $(
            scalar $sfname:ident ( $($sargt:tt),* ) -> $srett:tt
                [ $snullh:tt , $sdetkw:ident ]
                = $sbody:expr ;
        )*
        $(
            aggregate $afname:ident ( $($aargt:tt),* ) -> $arett:tt
                [ $adetkw:ident ]
            {
                state = $astate:ty ;
                init = $ainit:expr ;
                step = $astep:expr ;
                finalize = $afinal:expr ;
            }
        )+
    ) => {
        /// The generated extension core (one per crate). Carries the
        /// capability table + neutral dispatch; the duckdb aggregate shim
        /// is derived from this type alone.
        pub struct $core;

        impl $crate::ExtCore for $core {
            const NAME: &'static str = $name;
            const VERSION: &'static str = $version;
            const DECLS: &'static [$crate::FnDecl] = &[
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($sfname),
                        kind: $crate::CapabilityKind::Scalar,
                        args: &[ $( $crate::__ntype!($sargt) ),* ],
                        ret: $crate::__ntype!($srett),
                        null_handling: $crate::__nullh!($snullh),
                        deterministic: $crate::__declare_det!($sdetkw),
                        columns: &[],
                        replacement_scan_extensions: &[],
                    },
                )*
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($afname),
                        kind: $crate::CapabilityKind::Aggregate,
                        args: &[ $( $crate::__ntype!($aargt) ),* ],
                        ret: $crate::__ntype!($arett),
                        // Aggregates handle NULL per-row inside `step`
                        // (matching the hand-written `and_then` skip), so
                        // the shim never pre-filters: declare Called.
                        null_handling: $crate::NullHandling::Called,
                        deterministic: $crate::__declare_det!($adetkw),
                        columns: &[],
                        replacement_scan_extensions: &[],
                    },
                )+
            ];

            fn dispatch(
                idx: usize,
                args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let scalars: &[fn(&[$crate::NeutralValue])
                    -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                    $( $sbody ),*
                ];
                match scalars.get(idx) {
                    ::core::option::Option::Some(f) => f(args),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: function index {} is not a scalar", $name, idx)
                    ),
                }
            }

            fn dispatch_aggregate(
                idx: usize,
                rows: &[&[$crate::NeutralValue]],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                // The number of scalars precedes the aggregates in DECLS;
                // compute it from the same scalar list dispatch uses so it
                // can never drift.
                let scalar_count: usize = {
                    let scalars: &[fn(&[$crate::NeutralValue])
                        -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                        $( $sbody ),*
                    ];
                    scalars.len()
                };
                // One fold per declared aggregate, in DECLS (post-scalar)
                // order. Each composes init -> step* -> finalize entirely
                // in-guest over the buffered group.
                let folds: &[fn(&[&[$crate::NeutralValue]])
                    -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                    $(
                        |rows: &[&[$crate::NeutralValue]]|
                            -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>
                        {
                            #[allow(unused_imports)]
                            use $crate::ArgExt as _;
                            let mut state: $astate = $ainit;
                            let step = $astep;
                            for row in rows {
                                step(&mut state, *row);
                            }
                            let finalize = $afinal;
                            finalize(state)
                        }
                    ),+
                ];
                match idx.checked_sub(scalar_count).and_then(|a| folds.get(a)) {
                    ::core::option::Option::Some(f) => f(rows),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: function index {} is not an aggregate", $name, idx)
                    ),
                }
            }
        }

        impl $core {
            /// NUL-terminated SCALAR function names (DECLS order). Empty for
            /// a pure-aggregate core. Used by the (deferred) embed shim.
            #[allow(dead_code)]
            pub const SCALAR_NAMES_NUL: &'static [&'static [u8]] = &[
                $( ::core::concat!(::core::stringify!($sfname), "\0").as_bytes() ),*
            ];
        }
    };

    // ---- Pure-table form (T3) ----
    //
    // Every DECL is a `Table`; `dispatch` returns an error, `dispatch_table`
    // picks from a closure list indexed 1:1 with DECLS. The optional
    // `replacement_scan = [...]` list lifts to `FnDecl::replacement_scan_extensions`
    // for the ducklink shim to wire into the `files` interface.
    (
        core = $core:ident;
        extension = $name:expr;
        version = $version:expr;
        $(
            table $tfname:ident ( $($targt:tt),* $(,)? )
                -> ( $($tcolname:ident : $tcolt:tt),+ $(,)? )
                [ $tdetkw:ident $( , replacement_scan = [ $($trsext:expr),+ $(,)? ] )? ]
                = $tbody:expr ;
        )+
    ) => {
        /// The generated extension core (one per crate). Carries the
        /// capability table + neutral dispatch; the ducklink shim reads
        /// this type alone to drive registration + `call_table`.
        pub struct $core;

        impl $crate::ExtCore for $core {
            const NAME: &'static str = $name;
            const VERSION: &'static str = $version;
            const DECLS: &'static [$crate::FnDecl] = &[
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($tfname),
                        kind: $crate::CapabilityKind::Table,
                        args: &[ $( $crate::__ntype!($targt) ),* ],
                        // `ret` is unused for Table decls (per-column
                        // types live in `columns`); a cheap placeholder
                        // that needs no allocation inside the const.
                        ret: $crate::NeutralType::Blob,
                        // Table dispatch always calls the body — the core
                        // decides how to react to NULL inputs.
                        null_handling: $crate::NullHandling::Called,
                        deterministic: $crate::__declare_det!($tdetkw),
                        columns: &[
                            $(
                                $crate::ColDecl {
                                    name: ::core::stringify!($tcolname),
                                    ntype: $crate::__ntype!($tcolt),
                                },
                            )+
                        ],
                        // When `replacement_scan = [...]` is omitted the
                        // outer `$(...)?` block emits nothing, leaving an
                        // empty `&[]`. When present the exprs lift into
                        // `&["gb", "gbk"]` (or whatever was named).
                        replacement_scan_extensions: &[
                            $( $( $trsext ),+ )?
                        ],
                    },
                )+
            ];

            fn dispatch(
                idx: usize,
                _args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                ::core::result::Result::Err(::alloc::format!(
                    "{}: function index {} is not a scalar", $name, idx
                ))
            }

            fn dispatch_table(
                idx: usize,
                args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<
                ::alloc::vec::Vec<::alloc::vec::Vec<$crate::NeutralValue>>,
                ::alloc::string::String,
            > {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let handlers: &[fn(&[$crate::NeutralValue])
                    -> ::core::result::Result<
                        ::alloc::vec::Vec<::alloc::vec::Vec<$crate::NeutralValue>>,
                        ::alloc::string::String,
                    >] = &[
                    $( $tbody ),+
                ];
                match handlers.get(idx) {
                    ::core::option::Option::Some(f) => f(args),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!(
                            "{}: function index {} is not a table", $name, idx
                        )
                    ),
                }
            }
        }

        impl $core {
            /// NUL-terminated SCALAR function names (DECLS order). Empty
            /// for a pure-table core.
            #[allow(dead_code)]
            pub const SCALAR_NAMES_NUL: &'static [&'static [u8]] = &[];
        }
    };

    // ---- Mixed scalar + aggregate + table form ----
    //
    // Requires AT LEAST ONE `table ...;` (the `+` on the table block) so
    // this arm never fires for the existing scalar-only or
    // scalar+aggregate cores. DECLS layout: scalars, then aggregates,
    // then tables; each `dispatch*` fn shifts its idx by the counts
    // before it, exactly as the scalar+aggregate arm does today.
    (
        core = $core:ident;
        extension = $name:expr;
        version = $version:expr;
        $(
            scalar $sfname:ident ( $($sargt:tt),* ) -> $srett:tt
                [ $snullh:tt , $sdetkw:ident ]
                = $sbody:expr ;
        )*
        $(
            aggregate $afname:ident ( $($aargt:tt),* ) -> $arett:tt
                [ $adetkw:ident ]
            {
                state = $astate:ty ;
                init = $ainit:expr ;
                step = $astep:expr ;
                finalize = $afinal:expr ;
            }
        )*
        $(
            table $tfname:ident ( $($targt:tt),* $(,)? )
                -> ( $($tcolname:ident : $tcolt:tt),+ $(,)? )
                [ $tdetkw:ident $( , replacement_scan = [ $($trsext:expr),+ $(,)? ] )? ]
                = $tbody:expr ;
        )+
    ) => {
        pub struct $core;

        impl $crate::ExtCore for $core {
            const NAME: &'static str = $name;
            const VERSION: &'static str = $version;
            const DECLS: &'static [$crate::FnDecl] = &[
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($sfname),
                        kind: $crate::CapabilityKind::Scalar,
                        args: &[ $( $crate::__ntype!($sargt) ),* ],
                        ret: $crate::__ntype!($srett),
                        null_handling: $crate::__nullh!($snullh),
                        deterministic: $crate::__declare_det!($sdetkw),
                        columns: &[],
                        replacement_scan_extensions: &[],
                    },
                )*
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($afname),
                        kind: $crate::CapabilityKind::Aggregate,
                        args: &[ $( $crate::__ntype!($aargt) ),* ],
                        ret: $crate::__ntype!($arett),
                        null_handling: $crate::NullHandling::Called,
                        deterministic: $crate::__declare_det!($adetkw),
                        columns: &[],
                        replacement_scan_extensions: &[],
                    },
                )*
                $(
                    $crate::FnDecl {
                        name: ::core::stringify!($tfname),
                        kind: $crate::CapabilityKind::Table,
                        args: &[ $( $crate::__ntype!($targt) ),* ],
                        ret: $crate::NeutralType::Blob,
                        null_handling: $crate::NullHandling::Called,
                        deterministic: $crate::__declare_det!($tdetkw),
                        columns: &[
                            $(
                                $crate::ColDecl {
                                    name: ::core::stringify!($tcolname),
                                    ntype: $crate::__ntype!($tcolt),
                                },
                            )+
                        ],
                        replacement_scan_extensions: &[
                            $( $( $trsext ),+ )?
                        ],
                    },
                )+
            ];

            fn dispatch(
                idx: usize,
                args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let scalars: &[fn(&[$crate::NeutralValue])
                    -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                    $( $sbody ),*
                ];
                match scalars.get(idx) {
                    ::core::option::Option::Some(f) => f(args),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: function index {} is not a scalar", $name, idx)
                    ),
                }
            }

            fn dispatch_aggregate(
                idx: usize,
                rows: &[&[$crate::NeutralValue]],
            ) -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String> {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let scalar_count: usize = {
                    let scalars: &[fn(&[$crate::NeutralValue])
                        -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                        $( $sbody ),*
                    ];
                    scalars.len()
                };
                let folds: &[fn(&[&[$crate::NeutralValue]])
                    -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                    $(
                        |rows: &[&[$crate::NeutralValue]]|
                            -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>
                        {
                            #[allow(unused_imports)]
                            use $crate::ArgExt as _;
                            let mut state: $astate = $ainit;
                            let step = $astep;
                            for row in rows {
                                step(&mut state, *row);
                            }
                            let finalize = $afinal;
                            finalize(state)
                        }
                    ),*
                ];
                match idx.checked_sub(scalar_count).and_then(|a| folds.get(a)) {
                    ::core::option::Option::Some(f) => f(rows),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: function index {} is not an aggregate", $name, idx)
                    ),
                }
            }

            fn dispatch_table(
                idx: usize,
                args: &[$crate::NeutralValue],
            ) -> ::core::result::Result<
                ::alloc::vec::Vec<::alloc::vec::Vec<$crate::NeutralValue>>,
                ::alloc::string::String,
            > {
                #[allow(unused_imports)]
                use $crate::ArgExt as _;
                let scalar_count: usize = {
                    let scalars: &[fn(&[$crate::NeutralValue])
                        -> ::core::result::Result<$crate::NeutralValue, ::alloc::string::String>] = &[
                        $( $sbody ),*
                    ];
                    scalars.len()
                };
                let agg_count: usize = {
                    // Every aggregate contributes 1 to the count. A 1-per
                    // -decl marker sidesteps the `$astate` type referring
                    // back to its declaration-site closure captures.
                    #[allow(dead_code)]
                    const AGG_MARKERS: &[u8] = &[
                        $( { let _ = ::core::stringify!($afname); 0u8 } ),*
                    ];
                    AGG_MARKERS.len()
                };
                let handlers: &[fn(&[$crate::NeutralValue])
                    -> ::core::result::Result<
                        ::alloc::vec::Vec<::alloc::vec::Vec<$crate::NeutralValue>>,
                        ::alloc::string::String,
                    >] = &[
                    $( $tbody ),+
                ];
                match idx
                    .checked_sub(scalar_count + agg_count)
                    .and_then(|a| handlers.get(a))
                {
                    ::core::option::Option::Some(f) => f(args),
                    ::core::option::Option::None => ::core::result::Result::Err(
                        ::alloc::format!("{}: function index {} is not a table", $name, idx)
                    ),
                }
            }
        }

        impl $core {
            /// NUL-terminated SCALAR function names (DECLS order). Empty
            /// for a core with no scalars.
            #[allow(dead_code)]
            pub const SCALAR_NAMES_NUL: &'static [&'static [u8]] = &[
                $( ::core::concat!(::core::stringify!($sfname), "\0").as_bytes() ),*
            ];
        }
    };
}

/// Map a determinism keyword (`deterministic` / `nondeterministic`) to a
/// bool inside [`declare!`].
#[macro_export]
#[doc(hidden)]
macro_rules! __declare_det {
    (deterministic) => { true };
    (nondeterministic) => { false };
}
