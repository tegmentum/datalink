//! WIT identifier kebab-fix for digit-starting segments (#655, #756, #761).
//!
//! ## Why this exists
//!
//! The component-model kebab-case rule requires each `-`-separated
//! segment of an identifier to start with `[a-z]`. Several upstream
//! shim WIT packages (notably `sfcgal:component`'s geometry interface)
//! ship identifiers whose segments are digit-starting tokens like
//! `-2d` / `-3d` (e.g. `is-3d`, `translate-2d`, `intersects-3d`,
//! `cg-3d-alpha-wrapping`, `rotate-3d-around-center`) or bare single
//! digits like `-2` (from CGAL's C++ template naming — e.g.
//! `approx-convex-partition-2`, `y-monotone-partition-2`, mirroring
//! `Constrained_Delaunay_triangulation_2`). These names are accepted
//! by `wasmtime` and `wac plug` but rejected by strict `wit-bindgen`
//! (0.37+) and `jco` — and the bridge crates we emit consume
//! `wit-bindgen` at build time, so a freshly regen'd bridge fails
//! to compile against the unmodified upstream WIT.
//!
//! Datafission already has a wasm-level fix for the SAME identifiers
//! baked into the composed artifact (`scripts/fix-postgis-kebab.sh`,
//! invoked via the `postgis-composed-pin.txt` workflow). This module
//! is the WIT-source-text twin: when the codegen copies upstream
//! WIT into the bridge's `wit/deps/`, it rewrites `-2d` / `-3d` /
//! `-4d` and bare `-2` / `-3` / `-4` identifier segments to word
//! forms so the emitted text matches the kebab-fixed component's
//! extern names.
//!
//! ## Translation rules
//!
//! For each hyphen-separated segment of a kebab identifier:
//!   * `2d` → `twod`
//!   * `3d` → `threed`
//!   * `4d` → `fourd`
//!   * `2`  → `two`     (#761)
//!   * `3`  → `three`   (#761)
//!   * `4`  → `four`    (#761)
//!
//! Position is irrelevant — trailing (`is-3d`, `translate-2d`,
//! `approx-convex-partition-2`), mid-position (`cg-3d-alpha-wrapping`,
//! `union-3d-aggregate`, `rotate-3d-around-center`), and multiple
//! occurrences are all rewritten.
//!
//! Anchoring:
//!   * Only exact `Nd` / bare-`N` segments are rewritten. Multi-digit
//!     segments like `epsg-4326` or `srid-3857` are NOT rewritten
//!     here — they should be renamed upstream (e.g. `srid-webmercator`)
//!     because there's no unambiguous word form for arbitrary integers.
//!     `coordinate2d` (no hyphen) is a single non-matching segment,
//!     so it's left intact. `3dm` / `3dz` (extra chars after `Nd`)
//!     also don't match.
//!   * The identifier must contain at least one segment with a
//!     lowercase letter, so pure-digit sequences (unlikely in WIT
//!     but possible in commit SHAs like `a48ab3d` — which is a single
//!     non-hyphenated segment anyway) aren't touched.
//!
//! ## Audit surface (as of #761)
//!
//! Sweep of top-level WIT in `postgis-wasm/`, `mobilitydb-wasm/`, and
//! `sfcgal-wasm/` shows:
//!   * `-2d` / `-3d` / `-4d`: many, all handled since #655/#756.
//!   * bare trailing `-2`: 5 (partition-2 family in sfcgal geometry),
//!     handled by this pass (#761).
//!   * bare trailing `-3` / `-4`: none observed; handled prophylactically.
//!   * Digit-starting head segments (e.g. `2d-foo`): none observed.
//!   * Multi-digit tails (SRID codes, magic numbers): none observed
//!     in shim-facing WIT; would be flagged as upstream rename work.
//!
//! ## Invocation
//!
//! `kebab_fix_wit(content)` returns the rewritten text. The per-
//! target `emit_wit.rs` modules in `datalink-shim-{sqlite,duckdb,
//! datafission}-emit` call this from inside `copy_tree` whenever they
//! copy a `.wit` source file into the bridge's `wit/deps/`.

/// Apply the `-2d` / `-3d` / `-4d` → `-twod` / `-threed` / `-fourd`
/// and bare `-2` / `-3` / `-4` → `-two` / `-three` / `-four`
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

/// Translate any hyphen-separated `Nd` segment (2d/3d/4d) or bare
/// digit segment (2/3/4) inside a kebab identifier to its word form.
/// Non-matching segments and identifiers without a hyphen pass
/// through unchanged.
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
            // #761: bare single-digit segments from CGAL template names
            // (e.g. `partition-2`, `y-monotone-partition-2`). Only 2/3/4
            // are handled — multi-digit segments (SRID codes, magic
            // numbers) are left alone and should be renamed upstream.
            "2" => {
                translated.push("two".to_string());
                changed = true;
            }
            "3" => {
                translated.push("three".to_string());
                changed = true;
            }
            "4" => {
                translated.push("four".to_string());
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

    // ============================================
    // #761: trailing bare-digit segment tests
    // ============================================

    #[test]
    fn rewrites_trailing_bare_2() {
        // #761: `approx-convex-partition-2` from CGAL C++ template
        // `Constrained_Delaunay_triangulation_2`.
        let src = "approx-convex-partition-2: func(geom: geometry-handle) -> geometry-result;";
        let want = "approx-convex-partition-two: func(geom: geometry-handle) -> geometry-result;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_trailing_bare_2_full_family() {
        // #761: full partition-2 family from sfcgal-wasm/wit/geometry.wit.
        let src = "approx-convex-partition-2 greene-approx-convex-partition-2 \
                   optimal-convex-partition-2 y-monotone-partition-2";
        let want = "approx-convex-partition-two greene-approx-convex-partition-two \
                   optimal-convex-partition-two y-monotone-partition-two";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_trailing_bare_3_prophylactic() {
        // No observed identifier today, but symmetric with `2`.
        let src = "foo-3: func() -> bool;";
        let want = "foo-three: func() -> bool;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_trailing_bare_4_prophylactic() {
        // No observed identifier today, but symmetric with `2`.
        let src = "foo-4: func() -> bool;";
        let want = "foo-four: func() -> bool;";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn rewrites_mid_position_bare_digit() {
        // Contrived but supported by the segment-split rule.
        let src = "foo-2-bar";
        let want = "foo-two-bar";
        assert_eq!(kebab_fix_wit(src), want);
    }

    #[test]
    fn leaves_multi_digit_trailing_intact() {
        // Multi-digit tokens (SRID codes, magic numbers, EPSG identifiers)
        // are NOT rewritten — there's no unambiguous word form for `4326`,
        // and they should be renamed upstream to e.g. `srid-webmercator`.
        // wit-bindgen will still reject these, but the fix is upstream, not
        // here.
        let src = "epsg-4326 srid-3857 mercator-42";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn leaves_bare_digit_with_no_hyphen_intact() {
        // Standalone `2` in prose isn't a kebab segment.
        let src = "// 2 dimensions supported\n";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn leaves_pure_digit_kebab_intact() {
        // `1-2-3` is all digits — no lowercase-letter segment, so the
        // guard bails out.
        let src = "// see section 1-2-3\n";
        assert_eq!(kebab_fix_wit(src), src);
    }

    #[test]
    fn combines_bare_digit_and_nd_in_one_identifier() {
        // Hypothetical mixed case: `foo-2d-bar-2` — both segments rewritten.
        let src = "foo-2d-bar-2";
        let want = "foo-twod-bar-two";
        assert_eq!(kebab_fix_wit(src), want);
    }
}
