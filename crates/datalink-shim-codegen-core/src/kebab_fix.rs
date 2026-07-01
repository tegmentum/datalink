//! WIT identifier kebab-fix for digit-starting segments (#655, #756).
//!
//! ## Why this exists
//!
//! The component-model kebab-case rule requires each `-`-separated
//! segment of an identifier to start with `[a-z]`. Several upstream
//! shim WIT packages (notably `sfcgal:component`'s geometry interface)
//! ship identifiers whose segments are digit-starting tokens like
//! `-2d` / `-3d` (e.g. `is-3d`, `translate-2d`, `intersects-3d`,
//! `cg-3d-alpha-wrapping`, `rotate-3d-around-center`). These names
//! are accepted by `wasmtime` and `wac plug` but rejected by strict
//! `wit-bindgen` (0.37+) and `jco` — and the bridge crates we emit
//! consume `wit-bindgen` at build time, so a freshly regen'd bridge
//! fails to compile against the unmodified upstream WIT.
//!
//! Datafission already has a wasm-level fix for the SAME identifiers
//! baked into the composed artifact (`scripts/fix-postgis-kebab.sh`,
//! invoked via the `postgis-composed-pin.txt` workflow). This module
//! is the WIT-source-text twin: when the codegen copies upstream
//! WIT into the bridge's `wit/deps/`, it rewrites `-2d` / `-3d` /
//! `-4d` identifier segments to `-twod` / `-threed` / `-fourd` so
//! the emitted text matches the kebab-fixed component's extern
//! names.
//!
//! ## Translation rules
//!
//! For each hyphen-separated segment of a kebab identifier:
//!   * `2d` → `twod`
//!   * `3d` → `threed`
//!   * `4d` → `fourd`
//!
//! Position is irrelevant — trailing (`is-3d`, `translate-2d`),
//! mid-position (`cg-3d-alpha-wrapping`, `union-3d-aggregate`,
//! `rotate-3d-around-center`), and multiple occurrences are all
//! rewritten.
//!
//! Anchoring:
//!   * Only exact `Nd` segments are rewritten — `coordinate2d` (no
//!     hyphen) is a single non-matching segment, so it's left intact.
//!     `3dm` / `3dz` (extra chars after `Nd`) also don't match.
//!   * The identifier must contain at least one segment with a
//!     lowercase letter, so pure-digit sequences (unlikely in WIT
//!     but possible in commit SHAs like `a48ab3d` — which is a single
//!     non-hyphenated segment anyway) aren't touched.
//!
//! ## Invocation
//!
//! `kebab_fix_wit(content)` returns the rewritten text. The per-
//! target `emit_wit.rs` modules in `datalink-shim-{sqlite,duckdb,
//! datafission}-emit` call this from inside `copy_tree` whenever they
//! copy a `.wit` source file into the bridge's `wit/deps/`.

/// Apply the `-2d` / `-3d` / `-4d` → `-twod` / `-threed` / `-fourd`
/// kebab-fix across an entire WIT source file. Handles both trailing
/// and mid-position digit segments. Leaves all other identifiers and
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
                out.push_str(&rewrite_nd(&ident));
                ident.clear();
            }
            out.push(c);
        }
    }
    if !ident.is_empty() {
        out.push_str(&rewrite_nd(&ident));
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Translate any hyphen-separated `Nd` segment (2d/3d/4d) inside a
/// kebab identifier to its word form. Non-matching segments and
/// identifiers without a hyphen pass through unchanged.
fn rewrite_nd(ident: &str) -> String {
    // Without a hyphen there are no segments to translate — a bare
    // `3d` token has no leading `-` to signal a kebab segment, and
    // strings like `a48ab3d` are single segments that don't match.
    if !ident.contains('-') {
        return ident.to_string();
    }
    // Guard: at least one segment must contain a lowercase letter, so
    // this is a real kebab identifier rather than e.g. a version-like
    // sequence of digits joined by hyphens.
    let segments: Vec<&str> = ident.split('-').collect();
    if !segments
        .iter()
        .any(|s| s.chars().any(|c| c.is_ascii_lowercase()))
    {
        return ident.to_string();
    }
    let mut translated: Vec<String> = Vec::with_capacity(segments.len());
    let mut changed = false;
    for seg in &segments {
        match *seg {
            "2d" => {
                translated.push("twod".to_string());
                changed = true;
            }
            "3d" => {
                translated.push("threed".to_string());
                changed = true;
            }
            "4d" => {
                translated.push("fourd".to_string());
                changed = true;
            }
            other => translated.push(other.to_string()),
        }
    }
    if changed {
        translated.join("-")
    } else {
        ident.to_string()
    }
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
    fn rewrites_middle_position_3d() {
        // #756: `cg-3d-alpha-wrapping` has -3d- mid-position. Widened
        // rule rewrites it to `cg-threed-alpha-wrapping`.
        let src = "cg-3d-alpha-wrapping: func(wkb: list<u8>) -> result;";
        let want = "cg-threed-alpha-wrapping: func(wkb: list<u8>) -> result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_middle_position_2d() {
        let src = "rotate-2d-around-center: func(geom: geometry-handle) -> geometry-result;";
        let want = "rotate-twod-around-center: func(geom: geometry-handle) -> geometry-result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_union_3d_aggregate() {
        // Real upstream sfcgal-wasm identifier.
        let src = "union-3d-aggregate: func(geoms: list<geometry-handle>) -> geometry-result;";
        let want =
            "union-threed-aggregate: func(geoms: list<geometry-handle>) -> geometry-result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_all_dimension_words() {
        let src = "translate-2d translate-3d translate-4d";
        let want = "translate-twod translate-threed translate-fourd";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_multiple_occurrences_in_one_identifier() {
        // Contrived: `foo-2d-bar-3d-baz` — both mid segments translated.
        let src = "foo-2d-bar-3d-baz";
        let want = "foo-twod-bar-threed-baz";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn leaves_no_hyphen_before_digit_intact() {
        // `coordinate2d` is a single segment (no hyphen before the
        // digit) so `2d` is not a segment on its own. Untouched.
        let src = "coordinate2d point3d volume4d";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn leaves_partial_digit_tokens_intact() {
        // `3dm` / `3dz` / `2dp` contain more than the `Nd` pattern and
        // aren't rewritten. (Also, wit-bindgen would reject them; we
        // don't invent a translation the upstream fix script doesn't.)
        let src = "st-3dm-length st-3dz-area";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn leaves_commit_sha_like_tokens_intact() {
        // `a48ab3d` is a single non-hyphenated segment ending in `3d`.
        // The pre-#756 code would try `strip_suffix("-3d")` and get
        // nothing (no leading `-`), leaving it alone. The widened
        // segment-split rule also leaves it alone because `a48ab3d`
        // is not the exact segment `3d`.
        let src = "// bumped in commit a48ab3d (#679)\n";
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
