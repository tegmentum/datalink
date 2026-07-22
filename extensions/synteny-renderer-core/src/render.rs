//! Pure SVG synteny renderer.
//!
//! Independent of the wit-bindgen bindings so that native `cargo test`
//! runs against these types without pulling in wasm-only symbols. The
//! `lib.rs` Guest wrapper converts the generated bindings records into
//! the plain structs below and delegates here.
//!
//! # Layout (rough)
//!
//! Tracks stack vertically at fixed spacing. Each track has a label gutter
//! on the left, then a horizontal bar scaled to `track.length`. Features
//! are coloured rectangles positioned by (`start-position`, `end-position`);
//! `strand` becomes a chevron on the leading edge. Links are filled quad
//! ribbons connecting the query feature footprint on its track down to the
//! subject feature footprint on the subject's track.
//!
//! Enough to produce something recognisable-shaped when a real caller pipes
//! genes across a few genomes in; not enough to look pretty on the edge
//! cases called out at the bottom of this file.

use core::fmt::Write as _;

/// Plain-Rust mirror of the WIT `track` record.
#[derive(Debug, Clone)]
pub struct Track {
    pub track_id: String,
    pub label: String,
    pub length: u32,
}

/// Plain-Rust mirror of the WIT `feature` record. `strand` is `s8` in
/// the WIT (positive for +, negative for -, zero for unstranded).
#[derive(Debug, Clone)]
pub struct Feature {
    pub track_id: String,
    pub feature_id: String,
    pub start_position: u32,
    pub end_position: u32,
    pub strand: i8,
    pub colour: Option<String>,
    pub label: Option<String>,
}

/// Plain-Rust mirror of the WIT `link` record.
#[derive(Debug, Clone)]
pub struct Link {
    pub query_track: String,
    pub query_feature: String,
    pub subject_track: String,
    pub subject_feature: String,
    pub identity: f64,
    pub colour: Option<String>,
}

/// Plain-Rust mirror of the WIT `render-error` variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    InvalidModel(String),
    EmptyInput,
}

// ---- Layout constants ----------------------------------------------------
//
// All in SVG user units (≈ CSS pixels). The canvas is fixed-width; the
// caller can scale it downstream via the viewport if they want to. Height
// grows with track count.

const CANVAS_WIDTH: f64 = 1000.0;
const PADDING_X: f64 = 20.0;
const LABEL_GUTTER: f64 = 140.0;
const TRACK_TOP_MARGIN: f64 = 40.0;
const TRACK_STRIDE: f64 = 140.0;
const TRACK_BAR_HEIGHT: f64 = 6.0;
const FEATURE_HEIGHT: f64 = 26.0;
const CHEVRON_WIDTH: f64 = 8.0;
const BOTTOM_MARGIN: f64 = 40.0;

/// matplotlib tab10 — a safe qualitative palette for anonymous features.
const TAB10: &[&str] = &[
    "#1f77b4", "#ff7f0e", "#2ca02c", "#d62728", "#9467bd", "#8c564b", "#e377c2", "#7f7f7f",
    "#bcbd22", "#17becf",
];

const LINK_DEFAULT: &str = "#888888";
const TRACK_BAR_COLOUR: &str = "#4a4a4a";
const TEXT_COLOUR: &str = "#222222";

/// Main entry point. Returns the SVG document as UTF-8 bytes.
pub fn render_svg(
    tracks: &[Track],
    features: &[Feature],
    links: &[Link],
) -> Result<Vec<u8>, RenderError> {
    if tracks.is_empty() && features.is_empty() && links.is_empty() {
        return Err(RenderError::EmptyInput);
    }
    if tracks.is_empty() {
        return Err(RenderError::InvalidModel(
            "no tracks supplied but features/links present".to_string(),
        ));
    }

    // Track lookup for orientation checks + link resolution.
    let mut track_index: Vec<(&str, usize)> = Vec::with_capacity(tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        if track_index.iter().any(|(id, _)| *id == t.track_id.as_str()) {
            return Err(RenderError::InvalidModel(format!(
                "duplicate track-id '{}'",
                t.track_id
            )));
        }
        if t.length == 0 {
            return Err(RenderError::InvalidModel(format!(
                "track '{}' has zero length",
                t.track_id
            )));
        }
        track_index.push((t.track_id.as_str(), i));
    }

    // Every feature must anchor to an existing track. Coordinate sanity is
    // also checked here so downstream drawing math doesn't wrap on u32
    // underflow.
    for f in features {
        if !track_index.iter().any(|(id, _)| *id == f.track_id.as_str()) {
            return Err(RenderError::InvalidModel(format!(
                "feature '{}' references unknown track '{}'",
                f.feature_id, f.track_id
            )));
        }
        if f.end_position < f.start_position {
            return Err(RenderError::InvalidModel(format!(
                "feature '{}' end ({}) precedes start ({})",
                f.feature_id, f.end_position, f.start_position
            )));
        }
    }

    let height = TRACK_TOP_MARGIN + (tracks.len() as f64) * TRACK_STRIDE + BOTTOM_MARGIN;
    let drawing_width = CANVAS_WIDTH - LABEL_GUTTER - PADDING_X;

    let mut out = String::with_capacity(2048 + features.len() * 200 + links.len() * 200);
    write_header(&mut out, height);

    // Links go under features so glyphs stay legible where ribbons overlap.
    write_links(&mut out, tracks, features, links, drawing_width);
    write_tracks(&mut out, tracks, drawing_width);
    write_features(&mut out, tracks, features, drawing_width);

    out.push_str("</svg>\n");
    Ok(out.into_bytes())
}

fn write_header(out: &mut String, height: f64) {
    let _ = write!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {w:.1} {h:.1}\" \
         width=\"{w:.1}\" height=\"{h:.1}\" \
         font-family=\"sans-serif\" font-size=\"12\">\n",
        w = CANVAS_WIDTH,
        h = height,
    );
    // Light background so links/features don't need to fight a transparent
    // canvas in whichever viewer picks the SVG up.
    let _ = write!(
        out,
        "<rect x=\"0\" y=\"0\" width=\"{w:.1}\" height=\"{h:.1}\" fill=\"#ffffff\"/>\n",
        w = CANVAS_WIDTH,
        h = height,
    );
}

fn track_baseline_y(track_ordinal: usize) -> f64 {
    TRACK_TOP_MARGIN + (track_ordinal as f64) * TRACK_STRIDE + FEATURE_HEIGHT
}

fn write_tracks(out: &mut String, tracks: &[Track], drawing_width: f64) {
    for (i, t) in tracks.iter().enumerate() {
        let y = track_baseline_y(i);
        // Label in the gutter.
        let _ = write!(
            out,
            "<text x=\"{x:.1}\" y=\"{y:.1}\" fill=\"{c}\" text-anchor=\"start\" \
             dominant-baseline=\"middle\">{label} ({len:} bp)</text>\n",
            x = PADDING_X,
            y = y + FEATURE_HEIGHT / 2.0,
            c = TEXT_COLOUR,
            label = escape_xml(&t.label),
            len = t.length,
        );
        // Track bar spanning the drawing area.
        let _ = write!(
            out,
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" \
             fill=\"{c}\" rx=\"2\" ry=\"2\"/>\n",
            x = LABEL_GUTTER,
            y = y + FEATURE_HEIGHT / 2.0 - TRACK_BAR_HEIGHT / 2.0,
            w = drawing_width,
            h = TRACK_BAR_HEIGHT,
            c = TRACK_BAR_COLOUR,
        );
    }
}

fn write_features(out: &mut String, tracks: &[Track], features: &[Feature], drawing_width: f64) {
    for (i, f) in features.iter().enumerate() {
        // Guaranteed to resolve — validated in render_svg.
        let (track_ordinal, track) = tracks
            .iter()
            .enumerate()
            .find(|(_, t)| t.track_id == f.track_id)
            .expect("feature track already validated");

        let y = track_baseline_y(track_ordinal);
        let (x0, x1) = feature_x_range(f, track, drawing_width);
        let colour = f.colour.as_deref().unwrap_or_else(|| palette_pick(i));

        write_feature_glyph(out, x0, x1, y, f.strand, colour);

        if let Some(label) = &f.label {
            // Centre the text over the glyph; skip when the glyph is too
            // narrow — cramped labels do more harm than good.
            let glyph_width = x1 - x0;
            if glyph_width >= 24.0 {
                let cx = (x0 + x1) / 2.0;
                let _ = write!(
                    out,
                    "<text x=\"{cx:.1}\" y=\"{y:.1}\" fill=\"{c}\" \
                     text-anchor=\"middle\" font-size=\"10\">{label}</text>\n",
                    cx = cx,
                    y = y - 4.0,
                    c = TEXT_COLOUR,
                    label = escape_xml(label),
                );
            }
        }
    }
}

fn feature_x_range(f: &Feature, track: &Track, drawing_width: f64) -> (f64, f64) {
    // WIT positions are 1-indexed inclusive on both ends; convert to a
    // fraction of the track length.
    let start = f.start_position.saturating_sub(1) as f64;
    let end = f.end_position as f64;
    let len = track.length.max(1) as f64;
    let x0 = LABEL_GUTTER + (start / len).clamp(0.0, 1.0) * drawing_width;
    let mut x1 = LABEL_GUTTER + (end / len).clamp(0.0, 1.0) * drawing_width;
    // Ensure a minimum visible width so single-base features still render.
    if x1 - x0 < 2.0 {
        x1 = x0 + 2.0;
    }
    (x0, x1)
}

fn write_feature_glyph(out: &mut String, x0: f64, x1: f64, y: f64, strand: i8, colour: &str) {
    let top = y;
    let bot = y + FEATURE_HEIGHT;
    let mid = (top + bot) / 2.0;
    let body_w = (x1 - x0).max(2.0);
    let chev = CHEVRON_WIDTH.min(body_w * 0.4);

    // Chevron on the leading edge; unstranded features become plain rects.
    let path = match strand.signum() {
        1 => format!(
            "M{x0:.1} {top:.1} \
             H{xr:.1} \
             L{x1:.1} {mid:.1} \
             L{xr:.1} {bot:.1} \
             H{x0:.1} Z",
            x0 = x0,
            xr = x1 - chev,
            x1 = x1,
            top = top,
            bot = bot,
            mid = mid,
        ),
        -1 => format!(
            "M{x1:.1} {top:.1} \
             H{xl:.1} \
             L{x0:.1} {mid:.1} \
             L{xl:.1} {bot:.1} \
             H{x1:.1} Z",
            x0 = x0,
            xl = x0 + chev,
            x1 = x1,
            top = top,
            bot = bot,
            mid = mid,
        ),
        _ => format!(
            "M{x0:.1} {top:.1} H{x1:.1} V{bot:.1} H{x0:.1} Z",
            x0 = x0,
            x1 = x1,
            top = top,
            bot = bot,
        ),
    };

    let _ = write!(
        out,
        "<path d=\"{d}\" fill=\"{c}\" stroke=\"#333333\" stroke-width=\"0.75\"/>\n",
        d = path,
        c = colour,
    );
}

fn write_links(
    out: &mut String,
    tracks: &[Track],
    features: &[Feature],
    links: &[Link],
    drawing_width: f64,
) {
    for l in links {
        let Some((q_ord, q_track, q_feat)) = resolve_endpoint(tracks, features, &l.query_track, &l.query_feature) else {
            continue; // Silently drop dangling links — tolerable in a first pass.
        };
        let Some((s_ord, s_track, s_feat)) = resolve_endpoint(tracks, features, &l.subject_track, &l.subject_feature) else {
            continue;
        };

        let (qx0, qx1) = feature_x_range(q_feat, q_track, drawing_width);
        let (sx0, sx1) = feature_x_range(s_feat, s_track, drawing_width);

        let q_y_top = track_baseline_y(q_ord);
        let s_y_top = track_baseline_y(s_ord);
        // Attach the ribbon to the far edge of each glyph so the ribbon
        // and the glyph don't overpaint each other.
        let (q_edge, s_edge) = if q_ord < s_ord {
            (q_y_top + FEATURE_HEIGHT, s_y_top)
        } else if q_ord > s_ord {
            (q_y_top, s_y_top + FEATURE_HEIGHT)
        } else {
            // Same track — draw a shallow arc above it. Treated as an
            // opaque diagnostic for the caller; not pretty but not wrong.
            (q_y_top, s_y_top)
        };

        let colour = l.colour.as_deref().unwrap_or(LINK_DEFAULT);
        // Identity in [0, 1] or [0, 100] — accept both, clamp to something
        // sensible so faint high-noise links stay legible.
        let identity_norm = normalise_identity(l.identity);
        let opacity = 0.2 + 0.6 * identity_norm;

        // Straight-sided quadrilateral. TODO: cubic Bezier ribbon.
        let _ = write!(
            out,
            "<path d=\"M{a:.1} {ay:.1} L{b:.1} {ay:.1} L{d:.1} {by:.1} L{c:.1} {by:.1} Z\" \
             fill=\"{col}\" fill-opacity=\"{op:.3}\" stroke=\"none\"/>\n",
            a = qx0,
            b = qx1,
            c = sx0,
            d = sx1,
            ay = q_edge,
            by = s_edge,
            col = colour,
            op = opacity,
        );
    }
}

fn resolve_endpoint<'a>(
    tracks: &'a [Track],
    features: &'a [Feature],
    track_id: &str,
    feature_id: &str,
) -> Option<(usize, &'a Track, &'a Feature)> {
    let (ord, track) = tracks
        .iter()
        .enumerate()
        .find(|(_, t)| t.track_id == track_id)?;
    let feature = features
        .iter()
        .find(|f| f.track_id == track_id && f.feature_id == feature_id)?;
    Some((ord, track, feature))
}

fn normalise_identity(identity: f64) -> f64 {
    if identity.is_nan() {
        return 0.0;
    }
    let v = if identity > 1.0 { identity / 100.0 } else { identity };
    v.clamp(0.0, 1.0)
}

fn palette_pick(ordinal: usize) -> &'static str {
    TAB10[ordinal % TAB10.len()]
}

fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

// ---- Known limitations (TODO on second pass) ----------------------------
//
// * Very long tracks: fixed 1000px canvas means features on a Mbp-scale
//   genome collapse to sub-pixel widths. A `max-track-length` parameter
//   with a scale bar would fix this without breaking the current WIT.
// * Overlapping features: painted in feed order, no lane-packing. A
//   forward greedy row-assign would keep long operons legible.
// * Ribbons are straight-sided quads, not Bezier curves. Cheap first
//   pass; genuine synteny plots use cubic curves to flow between tracks.
// * Same-track links draw a degenerate zero-height ribbon; would want
//   an above-the-track arc.
// * Text overflow: labels are neither wrapped nor truncated. Wide labels
//   walk off the right of the label gutter.
// * Palette: only tab10; no colour-blind override, no legend.

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, len: u32) -> Track {
        Track {
            track_id: id.to_string(),
            label: format!("track {id}"),
            length: len,
        }
    }

    fn feature(track_id: &str, feature_id: &str, start: u32, end: u32, strand: i8) -> Feature {
        Feature {
            track_id: track_id.to_string(),
            feature_id: feature_id.to_string(),
            start_position: start,
            end_position: end,
            strand,
            colour: None,
            label: Some(feature_id.to_string()),
        }
    }

    #[test]
    fn empty_input_errors() {
        assert_eq!(render_svg(&[], &[], &[]), Err(RenderError::EmptyInput));
    }

    #[test]
    fn features_without_tracks_error() {
        let f = feature("t1", "geneA", 1, 100, 1);
        let err = render_svg(&[], &[f], &[]).unwrap_err();
        assert!(matches!(err, RenderError::InvalidModel(_)));
    }

    #[test]
    fn unknown_track_in_feature_errors() {
        let t = track("t1", 500);
        let f = feature("t-nope", "g1", 1, 50, 1);
        let err = render_svg(&[t], &[f], &[]).unwrap_err();
        match err {
            RenderError::InvalidModel(msg) => assert!(msg.contains("unknown track")),
            _ => panic!("expected InvalidModel"),
        }
    }

    #[test]
    fn happy_path_two_tracks_three_features_one_link() {
        let tracks = vec![track("t1", 1000), track("t2", 1200)];
        let features = vec![
            feature("t1", "g1", 100, 300, 1),
            feature("t1", "g2", 400, 600, -1),
            feature("t1", "g3", 700, 900, 1),
            feature("t2", "h1", 150, 350, 1),
            feature("t2", "h2", 500, 700, -1),
            feature("t2", "h3", 850, 1050, 1),
        ];
        let links = vec![Link {
            query_track: "t1".to_string(),
            query_feature: "g1".to_string(),
            subject_track: "t2".to_string(),
            subject_feature: "h1".to_string(),
            identity: 92.5,
            colour: None,
        }];

        let bytes = render_svg(&tracks, &features, &links).expect("render should succeed");
        let s = core::str::from_utf8(&bytes).expect("valid utf-8");

        // Structural sanity — an SVG document with recognisable pieces.
        assert!(s.starts_with("<svg"));
        assert!(s.trim_end().ends_with("</svg>"));
        // Track labels rendered.
        assert!(s.contains("track t1"));
        assert!(s.contains("track t2"));
        // Feature labels rendered.
        assert!(s.contains(">g1<"));
        assert!(s.contains(">h1<"));
        // At least one link ribbon path present.
        assert!(s.contains("fill-opacity"));
        // Default palette colour appeared for at least one glyph.
        assert!(s.contains("#1f77b4"));
    }

    #[test]
    fn xml_escapes_apply_in_labels() {
        let tracks = vec![Track {
            track_id: "t1".to_string(),
            label: "<i>weird</i> & \"quotes\"".to_string(),
            length: 500,
        }];
        let bytes = render_svg(&tracks, &[], &[]).expect("render");
        let s = core::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("&lt;i&gt;weird&lt;/i&gt; &amp; &quot;quotes&quot;"));
        assert!(!s.contains("<i>weird</i>"));
    }

    #[test]
    fn identity_normalises_fractional_and_percentage_inputs() {
        assert!((normalise_identity(0.5) - 0.5).abs() < 1e-9);
        assert!((normalise_identity(50.0) - 0.5).abs() < 1e-9);
        assert_eq!(normalise_identity(-1.0), 0.0);
        assert_eq!(normalise_identity(1000.0), 1.0);
        assert_eq!(normalise_identity(f64::NAN), 0.0);
    }
}
