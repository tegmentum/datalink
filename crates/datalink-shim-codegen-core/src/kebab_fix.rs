//! WIT identifier kebab-fix for digit-starting trailing segments
//! (#655).
//!
//! ## Why this exists
//!
//! The component-model kebab-case rule requires each `-`-separated
//! segment of an identifier to start with `[a-z]`. Several upstream
//! shim WIT packages (notably `sfcgal:component`'s geometry interface)
//! ship identifiers whose final segment is a digit-starting token
//! like `-2d` / `-3d` (e.g. `is-3d`, `translate-2d`,
//! `intersects-3d`). These names are accepted by `wasmtime` and
//! `wac plug` but rejected by strict `wit-bindgen` (0.37+) and
//! `jco` — and the bridge crates we emit consume `wit-bindgen` at
//! build time, so a freshly regen'd bridge fails to compile against
//! the unmodified upstream WIT.
//!
//! Datafission already has a wasm-level fix for the SAME identifiers
//! baked into the composed artifact (`scripts/fix-postgis-kebab.sh`,
//! invoked via the `postgis-composed-pin.txt` workflow). This module
//! is the WIT-source-text twin: when the codegen copies upstream
//! WIT into the bridge's `wit/deps/`, it rewrites trailing
//! `-2d` / `-3d` identifier segments to `-twod` / `-threed` so the
//! emitted text matches the kebab-fixed component's extern names.
//!
//! ## Translation rules
//!
//! For each kebab identifier in the source text:
//!   * `<stem>-2d` → `<stem>-twod`
//!   * `<stem>-3d` → `<stem>-threed`
//!
//! Anchoring:
//!   * Only the FULL identifier suffix is rewritten — `coordinate2d`
//!     (no hyphen) and `st-3d-extent` (digits not in trailing
//!     position) are left intact.
//!   * `<stem>` must contain at least one lowercase letter, so bare
//!     `2d` / `3d` tokens (unlikely in valid WIT) aren't touched.
//!
//! ## Invocation
//!
//! `kebab_fix_wit(content)` returns the rewritten text. The per-
//! target `emit_wit.rs` modules in `datalink-shim-{sqlite,duckdb,
//! datafission}-emit` call this from inside `copy_tree` whenever they
//! copy a `.wit` source file into the bridge's `wit/deps/`.

/// Apply the trailing `-2d` / `-3d` → `-twod` / `-threed` kebab-fix
/// across an entire WIT source file. Leaves all other identifiers and
/// non-identifier characters (including comments, whitespace,
/// punctuation) unchanged.
pub fn kebab_fix_wit(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut ident = String::new();
    for c in content.chars() {
        if is_ident_char(c) {
            ident.push(c);
        } else {
            if !ident.is_empty() {
                out.push_str(&rewrite_trailing_nd(&ident));
                ident.clear();
            }
            out.push(c);
        }
    }
    if !ident.is_empty() {
        out.push_str(&rewrite_trailing_nd(&ident));
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

fn rewrite_trailing_nd(ident: &str) -> String {
    for (suffix, repl) in [("-2d", "-twod"), ("-3d", "-threed")] {
        if let Some(stem) = ident.strip_suffix(suffix) {
            // Only rewrite when stem looks like a real kebab identifier
            // (has at least one lowercase letter). `coordinate2d` and
            // `st-3d-extent` don't reach here — the former has no
            // hyphen-2d/3d suffix, the latter doesn't END with -3d.
            if !stem.is_empty() && stem.chars().any(|c| c.is_ascii_lowercase()) {
                return format!("{stem}{repl}");
            }
        }
    }
    ident.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_trailing_3d() {
        let src = "is-3d: func(geom: geometry-handle) -> bool;";
        let want = "is-threed: func(geom: geometry-handle) -> bool;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_trailing_2d() {
        let src = "translate-2d: func(geom: geometry-handle) -> geometry-result;";
        let want = "translate-twod: func(geom: geometry-handle) -> geometry-result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_multi_segment_kebab() {
        let src = "sym-difference-3d: func(a: geometry-handle) -> geometry-result;";
        let want = "sym-difference-threed: func(a: geometry-handle) -> geometry-result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn leaves_no_hyphen_intact() {
        // `coordinate2d` / `point3d` have no hyphen before the digit,
        // they're already valid kebab identifiers.
        let src = "record coordinate2d { x: f64 }\nrecord coordinate3d { x: f64 }";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn leaves_middle_position_intact() {
        // `st-3d-extent` has -3d- in middle, not at end. Should be
        // untouched.
        let src = "/// rejects `st-3d-extent` form; matching\n";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn translates_in_comments_too() {
        // Comments containing standalone `is-3d` get translated too —
        // not ideal for prose but acceptable, since post-fix the prose
        // and the actual identifier refer to the same renamed symbol.
        let src = "/// Returns true when is-3d holds.\n";
        let want = "/// Returns true when is-threed holds.\n";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn preserves_whitespace_and_punct() {
        let src = "    is-3d:   func() -> bool;\n";
        let want = "    is-threed:   func() -> bool;\n";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn no_change_when_no_match() {
        let src = "interface geometry {\n    foo: func() -> bool;\n}\n";
        assert_eq!(kebab_fix_wit(src), src);
    }
}
