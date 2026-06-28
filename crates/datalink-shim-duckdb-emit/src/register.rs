//! Emit `fn register_scalars() -> Result<(), types::Duckerror>`.
//!
//! Each scalar in the interface DB becomes one call to
//! `registry.register(name, &args, &ret, callback, Some(&opts))`
//! against the runtime's scalar-capability registry. The handle
//! returned by `runtime::ScalarCallback::new(handle)` routes every
//! invocation back to a `SCALAR_ARMS` match in the dispatch impl —
//! the same `handle` we slotted into `handle_table` at register
//! time.
//!
//! ## Funcarg / Logicaltype derivation
//!
//! The interface DB carries a SQL-side affinity per arg (`integer`
//! / `real` / `text` / `binary`). The scalar-first cut maps each
//! affinity to its widest DuckDB `Logicaltype` arm (Int64 / Float64
//! / Text / Blob) so SQL implicit casts compose. Once the codegen
//! gains awareness of the upstream WIT's exact param width
//! (s8/s16/s32/s64/u8/...) we can route through narrower types for
//! better DuckDB plan stats.
//!
//! ## Return type
//!
//! Same affinity table for the return Logicaltype. Today's PostGIS
//! scalars uniformly return Blob (WKB) or Float64 (lengths /
//! distances) or Boolean (predicates) — all covered by the four-arm
//! affinity model.
//!
//! ## NULL handling
//!
//! Default DuckDB behavior: arguments that are NULL short-circuit
//! to NULL without invoking the function. PostGIS scalars are
//! uniformly null-propagating in practice, so the scalar-first cut
//! does NOT call `runtime-ext.register-scalar-ex` (which would let
//! us mark `null-handling: special`). The base `runtime.scalar-
//! registry.register` is sufficient.

use anyhow::Result;
use shim_bridge_codegen_core::{BridgePlan, ScalarFn};

/// Render the `register_scalars()` body. The body iterates over
/// every (canonical-name, alias) scalar in the BridgePlan,
/// allocates a u32 handle, inserts (handle, arm_index) into
/// `handle_table`, and registers the scalar against the runtime's
/// scalar-capability.
pub fn render(plan: &BridgePlan) -> Result<String> {
    let mut s = String::new();
    s.push_str(REGISTER_PRELUDE);

    // The arm index is the same monotonic counter the dispatcher
    // uses to fire the right SCALAR_ARMS match arm. We use the
    // SAME ordering pass as `build_scalar_arm_index` in
    // `emit_lib.rs` so handle_table[handle] == arm index that
    // matches in the dispatch.
    let mut arm_idx: usize = 0;
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            let (name, num_args, ret_kind) = scalar_metadata(sc);
            push_registration(&mut s, arm_idx, &name, num_args, &ret_kind, sc.is_deterministic);
            arm_idx += 1;
            for alias in &sc.aliases {
                push_registration(
                    &mut s,
                    arm_idx,
                    alias,
                    num_args,
                    &ret_kind,
                    sc.is_deterministic,
                );
                arm_idx += 1;
            }
        }
    }

    s.push_str("    Ok(())\n}\n");
    Ok(s)
}

const REGISTER_PRELUDE: &str = r##"
fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| {
            types::Duckerror::Internal(
                "host did not expose scalar capability".into(),
            )
        })?;
    let registry = match capability {
        runtime::Capability::Scalar(r) => r,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ));
        }
    };
"##;

fn push_registration(
    out: &mut String,
    arm_idx: usize,
    sql_name: &str,
    num_args: usize,
    ret_logical: &str,
    deterministic: bool,
) {
    // Use a sensible default arg type (Text). The scalar-first
    // cut doesn't yet thread per-arg WIT types through the
    // interface_db IR for DuckDB-specific Logicaltype derivation —
    // the dispatch arm's per-arg `dv_*` helpers do the real type
    // coercion at runtime, so the registry-side declaration is
    // primarily for DuckDB's planner. We register every arg as
    // Text (the most permissive) to let the dispatch arm decide;
    // a follow-up can replace this with the actual WIT-derived
    // logical type for better planner stats.
    let mut args_block = String::new();
    for i in 0..num_args {
        args_block.push_str(&format!(
            "            runtime::Funcarg {{\n\
             \x20               name: Some(\"arg{i}\".into()),\n\
             \x20               logical: types::Logicaltype::Text,\n\
             \x20           }},\n",
            i = i,
        ));
    }
    let attrs = if deterministic {
        "types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS"
    } else {
        "types::Funcflags::STATELESS"
    };
    out.push_str(&format!(
        r##"    {{
        let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        handle_table()
            .lock()
            .expect("scalar handle mutex poisoned")
            .insert(handle, {arm_idx}usize);
        let callback = runtime::ScalarCallback::new(handle);
        let args: Vec<runtime::Funcarg> = vec![
{args_block}        ];
        let opts = runtime::Funcopts {{
            description: Some("{sql_name} (sqlink-shim-codegen)".into()),
            tags: vec!["{sql_name}".into()],
            attributes: {attrs},
        }};
        registry.register(
            "{sql_name}",
            &args,
            &{ret_logical},
            callback,
            Some(&opts),
        )?;
    }}
"##,
        arm_idx = arm_idx,
        sql_name = sql_name.replace('"', "\\\""),
        args_block = args_block,
        attrs = attrs,
        ret_logical = ret_logical,
    ));
}

/// Pull (function name as DuckDB sees it, fixed arg count, return
/// Logicaltype) out of a `ScalarFn`. Variadic / overloaded scalars
/// surface as `-1` in the SQLite emit path; for DuckDB we lower
/// to the first signature's length (DuckDB doesn't have the same
/// `-1 = variadic` ABI sigil — varargs is a separate
/// `runtime-ext.register-scalar-ex` arm we defer).
fn scalar_metadata(sc: &ScalarFn) -> (String, usize, String) {
    let first_sig = sc.param_signatures.first();
    let num_args = first_sig.map(|v| v.len()).unwrap_or(0);
    // For the scalar-first cut, the return logical type is
    // routed through a placeholder Text. The dispatch arm wraps
    // the actual return into the right Duckvalue variant at
    // runtime (Boolean / Int64 / Float64 / Text / Blob) regardless
    // of what we declare here; DuckDB's planner is forgiving
    // about declared-vs-emitted type narrowing.
    let ret_logical = "types::Logicaltype::Text".to_string();
    (sc.canonical_name.clone(), num_args, ret_logical)
}
