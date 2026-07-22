//! Neutral core for the `synteny-renderer` extension. One table function:
//!
//! - `render_synteny_svg(tracks VARCHAR, features VARCHAR, links VARCHAR) ->
//!   (svg BLOB, bytes_len INT64)` — three JSON-string args carrying the
//!   declarative intermediate model (`Track` / `Feature` / `Link` lists),
//!   returns exactly one row with the rendered SVG bytes and their length.
//!
//! On `RenderError::EmptyInput` the function returns ZERO rows (not a SQL
//! exception) — this lets callers write `SELECT * FROM
//! render_synteny_svg(NULL, NULL, NULL)` without special-casing.
//!
//! Renderer is not `no_std` — it wants alloc + std for string formatting.

// The declare! macro emits `alloc::vec!` / `alloc::string::String`.
extern crate alloc;

pub mod render;

use datalink_extcore::NeutralValue;
use serde::Deserialize;

use crate::render::{Feature, Link, RenderError, Track};

datalink_extcore::declare! {
    core = Core;
    extension = "synteny-renderer";
    version = env!("CARGO_PKG_VERSION");

    table render_synteny_svg(text, text, text) -> (
        svg: blob,
        bytes_len: int64,
    ) [deterministic] = |args| {
        use datalink_extcore::ArgExt as _;

        let tracks_j   = args.arg_text(0, "render_synteny_svg")?;
        let features_j = args.arg_text(1, "render_synteny_svg")?;
        let links_j    = args.arg_text(2, "render_synteny_svg")?;

        let tracks   = parse_json_list::<RawTrack>(&tracks_j, "tracks")?;
        let features = parse_json_list::<RawFeature>(&features_j, "features")?;
        let links    = parse_json_list::<RawLink>(&links_j, "links")?;

        let tracks: Vec<Track> = tracks.into_iter().map(Into::into).collect();
        let features: Vec<Feature> = features.into_iter().map(Into::into).collect();
        let links: Vec<Link> = links.into_iter().map(Into::into).collect();

        match render::render_svg(&tracks, &features, &links) {
            Ok(bytes) => {
                let bytes_len = bytes.len() as i64;
                Ok(vec![vec![NeutralValue::Blob(bytes), NeutralValue::Int64(bytes_len)]])
            }
            Err(RenderError::EmptyInput) => Ok(Vec::new()),
            Err(RenderError::InvalidModel(msg)) => {
                Err(format!("synteny-renderer: {msg}"))
            }
        }
    };
}

fn parse_json_list<T: for<'de> Deserialize<'de>>(json: &str, name: &str) -> Result<Vec<T>, String> {
    if json.is_empty() || json == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str(json)
        .map_err(|e| format!("synteny-renderer: cannot parse '{name}' JSON: {e}"))
}

// ---- serde types for the three JSON args ------------------------------

#[derive(Deserialize)]
struct RawTrack {
    track_id: String,
    label: String,
    length: u32,
}

impl From<RawTrack> for Track {
    fn from(r: RawTrack) -> Self {
        Track {
            track_id: r.track_id,
            label: r.label,
            length: r.length,
        }
    }
}

#[derive(Deserialize)]
struct RawFeature {
    track_id: String,
    feature_id: String,
    start_position: u32,
    end_position: u32,
    strand: i8,
    #[serde(default)]
    colour: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

impl From<RawFeature> for Feature {
    fn from(r: RawFeature) -> Self {
        Feature {
            track_id: r.track_id,
            feature_id: r.feature_id,
            start_position: r.start_position,
            end_position: r.end_position,
            strand: r.strand,
            colour: r.colour,
            label: r.label,
        }
    }
}

#[derive(Deserialize)]
struct RawLink {
    query_track: String,
    query_feature: String,
    subject_track: String,
    subject_feature: String,
    identity: f64,
    #[serde(default)]
    colour: Option<String>,
}

impl From<RawLink> for Link {
    fn from(r: RawLink) -> Self {
        Link {
            query_track: r.query_track,
            query_feature: r.query_feature,
            subject_track: r.subject_track,
            subject_feature: r.subject_feature,
            identity: r.identity,
            colour: r.colour,
        }
    }
}
