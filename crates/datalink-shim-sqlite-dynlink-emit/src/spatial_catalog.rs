//! Parser + resolver for `spatial-catalog.toml`.
//!
//! Phase A shape (per §A.3 of the Spatial-Catalog Integration
//! design). The catalog is the authoritative extension surface:
//! it lists every leaf (a cohesive SQL-facing bundle of scalars,
//! aggregates, table functions, window functions, operators, and
//! casts), the umbrella-to-leaves expansion tables, canonical /
//! alias mappings, and metadata about the extension (name +
//! version + logical-type ids).
//!
//! For a dynlink bridge, the catalog *is* the plan: the bridge
//! doesn't need a `.sqlite` interface DB. Every function name it
//! must advertise + dispatch on is a member of one of the leaves
//! that a `target` (leaf-id or umbrella-id) expands to.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Top-level TOML shape.
#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub schema_version: String,
    pub meta: Meta,
    #[serde(default)]
    pub types: Vec<TypeEntry>,
    #[serde(default, rename = "leaves")]
    pub leaves_vec: Vec<Leaf>,
    #[serde(default, rename = "aliases")]
    pub aliases: Vec<Alias>,
    #[serde(default, rename = "umbrellas")]
    pub umbrellas_vec: Vec<Umbrella>,

    // Rebuilt at load time (skipped on deserialisation).
    #[serde(skip)]
    pub leaves: HashMap<String, Leaf>,
    #[serde(skip)]
    pub umbrellas: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub extension: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub logical_types: HashMap<String, i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypeEntry {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub logical_id: Option<i64>,
    #[serde(default)]
    pub record_id: Option<i64>,
    #[serde(default)]
    pub owning_leaf: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// A single leaf. The TOML enum-of-lists shape carries name-only
/// entries for functions (scalar / aggregate / table / window) and
/// pre-parsed operator + cast rows.
#[derive(Debug, Clone, Deserialize)]
pub struct Leaf {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub scalars: Vec<String>,
    #[serde(default)]
    pub aggregates: Vec<String>,
    #[serde(default)]
    pub table_functions: Vec<String>,
    #[serde(default)]
    pub window_functions: Vec<String>,
    #[serde(default)]
    pub operators: Vec<toml::Value>,
    #[serde(default)]
    pub casts: Vec<toml::Value>,
    #[serde(default)]
    pub preprocessor_patterns: Vec<toml::Value>,
    #[serde(default)]
    pub spatial_indexes: Vec<toml::Value>,
    #[serde(default)]
    pub owns_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Alias {
    pub kind: String,
    pub canonical: String,
    pub alias: String,
    #[serde(default)]
    pub leaf: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Umbrella {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub expands_to: Vec<String>,
}

/// Optional overlay carrying per-leaf schema-interface hints.
/// The dynlink bridge only needs `resolve-by-id(provider_id)`; the
/// overlay is a codegen concern for the composed provider and is
/// accepted here for API symmetry with the design spec (§A.3).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LeavesOverlay {
    #[serde(default)]
    pub schema_interfaces: HashMap<String, Vec<String>>,
}

/// Categories of functions the emitter enumerates. The dispatch
/// arm shape is identical for all four in Phase A (opaque blob in /
/// blob out), so the kind is only carried so callers can tell
/// scalar arms apart from aggregate / table / window arms when
/// they matter (e.g. when the target contract has separate scalar
/// vs. aggregate registries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FnKind {
    Scalar,
    Aggregate,
    Table,
    Window,
}

/// Load a catalog from disk and materialise the flat `leaves` +
/// `umbrellas` lookup maps.
pub fn load(path: &Path) -> Result<Catalog> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spatial-catalog: {}", path.display()))?;
    let mut catalog: Catalog = toml::from_str(&text)
        .with_context(|| format!("parsing spatial-catalog: {}", path.display()))?;
    catalog.leaves = catalog
        .leaves_vec
        .iter()
        .cloned()
        .map(|l| (l.id.clone(), l))
        .collect();
    catalog.umbrellas = catalog
        .umbrellas_vec
        .iter()
        .map(|u| (u.id.clone(), u.expands_to.clone()))
        .collect();
    Ok(catalog)
}

/// Load the optional leaves-overlay TOML.
pub fn load_leaves_overlay(path: &Path) -> Result<LeavesOverlay> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading leaves overlay: {}", path.display()))?;
    let overlay: LeavesOverlay = toml::from_str(&text)
        .with_context(|| format!("parsing leaves overlay: {}", path.display()))?;
    Ok(overlay)
}

impl Catalog {
    /// Expand an umbrella-or-leaf target id into its constituent
    /// leaf ids. Umbrellas expand to their `expands_to` list; leaves
    /// map to a single-element vec; unknown ids error.
    pub fn resolve(&self, target: &str) -> Result<Vec<String>> {
        if let Some(leaves) = self.umbrellas.get(target) {
            return Ok(leaves.clone());
        }
        if self.leaves.contains_key(target) {
            return Ok(vec![target.to_string()]);
        }
        Err(anyhow!(
            "target '{}' is neither a leaf nor an umbrella in the catalog",
            target
        ))
    }

    /// Union of every function of every requested kind across the
    /// resolved leaves. Returned deduped + sorted (BTreeSet).
    pub fn functions_for(&self, leaves: &[String]) -> BTreeSet<(FnKind, String)> {
        let mut out = BTreeSet::new();
        for leaf_id in leaves {
            let Some(leaf) = self.leaves.get(leaf_id) else {
                continue;
            };
            for name in &leaf.scalars {
                out.insert((FnKind::Scalar, name.clone()));
            }
            for name in &leaf.aggregates {
                out.insert((FnKind::Aggregate, name.clone()));
            }
            for name in &leaf.table_functions {
                out.insert((FnKind::Table, name.clone()));
            }
            for name in &leaf.window_functions {
                out.insert((FnKind::Window, name.clone()));
            }
        }
        out
    }

    /// Every type owned by any of the resolved leaves.
    pub fn types_for(&self, leaves: &[String]) -> Vec<&TypeEntry> {
        let mut owning: BTreeSet<&str> = BTreeSet::new();
        for leaf_id in leaves {
            if let Some(leaf) = self.leaves.get(leaf_id) {
                for name in &leaf.owns_types {
                    owning.insert(name.as_str());
                }
            }
        }
        self.types
            .iter()
            .filter(|t| owning.contains(t.name.as_str()))
            .collect()
    }

    /// Aliases whose canonical form equals `canonical`.
    pub fn aliases_for_canonical(&self, canonical: &str) -> Vec<&Alias> {
        self.aliases
            .iter()
            .filter(|a| a.canonical == canonical)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_missing_target_errors() {
        let catalog = Catalog {
            schema_version: "1".into(),
            meta: Meta {
                extension: "postgis".into(),
                version: "0.1.0".into(),
                api_version: "0.1.0".into(),
                logical_types: Default::default(),
            },
            types: Vec::new(),
            leaves_vec: Vec::new(),
            aliases: Vec::new(),
            umbrellas_vec: Vec::new(),
            leaves: Default::default(),
            umbrellas: Default::default(),
        };
        assert!(catalog.resolve("nope").is_err());
    }
}
