//! The `declare!` macro: an extension core names its capability table
//! once, and gets the [`ExtCore`](crate::ExtCore) impl (the `DECLS`
//! slice + the neutral `dispatch`) for free. The per-DB shims are then
//! generated from that single declaration.

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

/// Declare an extension core: its name/version and a list of scalar
/// functions, each with neutral arg types, a neutral return type, a
/// null-handling contract, a determinism flag, and a body closure over
/// `&[NeutralValue] -> Result<NeutralValue, String>`.
///
/// Expands to a `struct Core` implementing
/// [`ExtCore`](crate::ExtCore). Both shim macros take `Core` and derive
/// the full per-DB glue from `Core::DECLS` + `Core::dispatch`.
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
#[macro_export]
macro_rules! declare {
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
}

/// Map a determinism keyword (`deterministic` / `nondeterministic`) to a
/// bool inside [`declare!`].
#[macro_export]
#[doc(hidden)]
macro_rules! __declare_det {
    (deterministic) => { true };
    (nondeterministic) => { false };
}
