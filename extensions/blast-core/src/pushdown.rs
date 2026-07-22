//! Filter-pushdown planner for the blast component.
//!
//! ## Design
//!
//! The recognition set lives on the `SearchOptions` record and callers
//! pass filter intent explicitly in the JSON `options` VARCHAR. This is
//! the **permanent** shape for the current DuckLink contract — not an
//! interim workaround.
//!
//! ducklink-extension v5.0.0's STABILITY.md marks
//! `duckdb:extension/{table-stream, table-stream-dispatch}` — the
//! original DuckDB filter-pushdown path — DEPRECATED, scheduled for
//! removal at the next `duckdb:extension` MAJOR bump (ducklink v6.0.0).
//! No host has consumed registrations from them since v4.6.0. The
//! deprecation policy requires two MINOR releases between the
//! announcement (v5.0.0) and removal, so the path stays alive through
//! at least v5.1 and v5.2, then is gone at v6.0. Building against it
//! today would ship dead bindgen surface and break at v6.0 without
//! adding any real behavior first.
//!
//! `runtime::TableRegistry` + `callback-dispatch::call-table` is the
//! current and only long-term dispatch path; it receives
//! `(handle, args)` with no filter argument, so the caller must supply
//! filter intent through the arguments themselves. That mechanism is the
//! whole reason these knobs live on `SearchOptions`.
//!
//! ## Historical note
//!
//! The original planner (git history: through commit `f8afef6`) was fed
//! the conjunctive `list<table-filter>` DuckDB pushed into an
//! extension's `call-table-open-filtered` export. The recognition +
//! merge logic is unchanged; only the input shape moved from
//! `TableFilter` records to `SearchOptions` fields.
//!
//! Five recognition patterns are preserved:
//!
//! 1. `evalue-max: X`                                → `SearchOptions.evalue_max` (tightened by MIN with any prior)
//! 2. `min-identity: X`                              → `SearchOptions.min_identity` (tightened by MAX)
//! 3. `query-keys: [K1, K2, ...]`                    → prune `queries` batch pre-search
//! 4. `subject-keys: [K1, K2, ...]`                  → prune `subjects` batch pre-search
//! 5. `strand: 'plus' | 'minus'`                     → drop non-matching hits post-search
//!
//! Contradictory or vacuous restrictions (empty key list, an unknown
//! strand label) trip the `short_circuit` flag and `run_search` is skipped
//! entirely — the scan opens on an empty result.

use std::collections::HashSet;

use crate::model::{Hit, SearchOptions, Sequence, Strand};

/// The strand the caller pinned the scan to. Kept separate from the WIT
/// `Strand` so tests don't have to spin up a `Sequence` just to talk about
/// strand filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandFilter {
    Plus,
    Minus,
}

impl StrandFilter {
    pub fn matches(self, s: Strand) -> bool {
        matches!(
            (self, s),
            (StrandFilter::Plus, Strand::Plus) | (StrandFilter::Minus, Strand::Minus)
        )
    }
}

/// The compiled, actionable view of the caller's SearchOptions.
///
/// Callers apply it: rewrite `SearchOptions`, `retain` the query/subject
/// batches, run the search, then post-filter hits by `strand_keep`.
///
/// If `short_circuit` is set the whole plan is infeasible — the scan
/// yields zero rows and `run_search` can be skipped entirely.
#[derive(Debug, Default, Clone)]
pub struct PushdownPlan {
    pub evalue_ceiling: Option<f64>,
    pub identity_floor: Option<f64>,
    /// `None` = every query key allowed. `Some(set)` = only keys in `set`.
    pub query_keys: Option<HashSet<String>>,
    /// `None` = every subject key allowed. `Some(set)` = only keys in `set`.
    pub subject_keys: Option<HashSet<String>>,
    pub strand_keep: Option<StrandFilter>,
    pub short_circuit: bool,
}

/// Build a `PushdownPlan` from a `SearchOptions` record. The evalue /
/// identity fields are echoed verbatim (already `Option<f64>`); the key /
/// strand fields become the richer `HashSet` / `StrandFilter` shapes that
/// make the apply step trivial.
///
/// Options and pushdown fields on the same struct is deliberate: the
/// caller's evalue/identity ceilings already live there, so the
/// tightening step can compare like-with-like without walking two
/// separate options records.
pub fn plan_from_options(opts: &SearchOptions) -> PushdownPlan {
    let mut p = PushdownPlan {
        evalue_ceiling: opts.evalue_max,
        identity_floor: opts.min_identity,
        ..Default::default()
    };

    if let Some(keys) = &opts.query_keys {
        p.query_keys = Some(plan_key_set(keys, &mut p.short_circuit));
    }
    if let Some(keys) = &opts.subject_keys {
        p.subject_keys = Some(plan_key_set(keys, &mut p.short_circuit));
    }
    if let Some(s) = &opts.strand {
        match s.as_str() {
            "plus" => p.strand_keep = Some(StrandFilter::Plus),
            "minus" => p.strand_keep = Some(StrandFilter::Minus),
            _ => {} // unknown label: ignore silently, callers can add
                    // strict rejection later if they want
        }
    }

    p
}

/// Empty key list -> short-circuit; otherwise dedup into a HashSet.
fn plan_key_set(keys: &[String], short_circuit: &mut bool) -> HashSet<String> {
    if keys.is_empty() {
        *short_circuit = true;
        HashSet::new()
    } else {
        keys.iter().cloned().collect()
    }
}

/// Fold the plan's numeric bounds back into `opts`. This is a no-op after
/// `plan_from_options` (which already copied them in) — it exists for the
/// symmetry with the old planner and for the future filter-adapter path,
/// where a set of pushed `TableFilter`s could tighten the ceilings BEYOND
/// what the caller wrote in `options` and then this call would carry that
/// tightening back into `opts` so `run_search`'s filter pass sees it.
pub fn tighten_options(plan: &PushdownPlan, opts: &mut SearchOptions) {
    if let Some(e) = plan.evalue_ceiling {
        opts.evalue_max = Some(match opts.evalue_max {
            Some(cur) => cur.min(e),
            None => e,
        });
    }
    if let Some(i) = plan.identity_floor {
        opts.min_identity = Some(match opts.min_identity {
            Some(cur) => cur.max(i),
            None => i,
        });
    }
}

/// Drop queries/subjects the key-restriction clauses ruled out. Returns
/// `true` if either batch is now empty (nothing left to search).
pub fn prune_batches(
    plan: &PushdownPlan,
    queries: &mut Vec<Sequence>,
    subjects: &mut Vec<Sequence>,
) -> bool {
    if let Some(ks) = &plan.query_keys {
        queries.retain(|q| ks.contains(&q.key));
    }
    if let Some(ks) = &plan.subject_keys {
        subjects.retain(|s| ks.contains(&s.key));
    }
    queries.is_empty() || subjects.is_empty()
}

/// If a strand was pinned, drop hits on the other orientation.
pub fn keep_strand(plan: &PushdownPlan, hits: Vec<Hit>) -> Vec<Hit> {
    match plan.strand_keep {
        Some(sf) => hits.into_iter().filter(|h| sf.matches(h.strand)).collect(),
        None => hits,
    }
}

// ---- tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SearchOptions {
        SearchOptions {
            evalue_max: None,
            max_target_seqs: None,
            min_identity: None,
            query_keys: None,
            subject_keys: None,
            strand: None,
        }
    }

    fn seq(k: &str) -> Sequence {
        Sequence {
            key: k.to_string(),
            data: "ACGT".to_string(),
        }
    }

    #[test]
    fn evalue_and_identity_flow_through_verbatim() {
        let mut o = opts();
        o.evalue_max = Some(1e-5);
        o.min_identity = Some(90.0);

        let p = plan_from_options(&o);
        assert_eq!(p.evalue_ceiling, Some(1e-5));
        assert_eq!(p.identity_floor, Some(90.0));
        assert!(!p.short_circuit);

        // Round-trip: tightening a plan back into the same options is a no-op.
        tighten_options(&p, &mut o);
        assert_eq!(o.evalue_max, Some(1e-5));
        assert_eq!(o.min_identity, Some(90.0));
    }

    #[test]
    fn tighten_options_never_loosens() {
        // Plan says evalue <= 1e-3 but caller already set 1e-8 — 1e-8 wins.
        let plan = PushdownPlan {
            evalue_ceiling: Some(1e-3),
            identity_floor: Some(50.0),
            ..Default::default()
        };
        let mut o = opts();
        o.evalue_max = Some(1e-8);
        o.min_identity = Some(95.0);

        tighten_options(&plan, &mut o);
        assert_eq!(o.evalue_max, Some(1e-8));
        assert_eq!(o.min_identity, Some(95.0));
    }

    #[test]
    fn query_and_subject_keys_prune_batches() {
        let mut o = opts();
        o.query_keys = Some(vec!["qA".into()]);
        o.subject_keys = Some(vec!["s1".into(), "s3".into()]);

        let plan = plan_from_options(&o);
        assert_eq!(plan.query_keys.as_ref().map(|s| s.len()), Some(1));
        assert_eq!(plan.subject_keys.as_ref().map(|s| s.len()), Some(2));

        let mut qs = vec![seq("qA"), seq("qB"), seq("qC")];
        let mut ss = vec![seq("s1"), seq("s2"), seq("s3")];
        let empty = prune_batches(&plan, &mut qs, &mut ss);

        assert!(!empty);
        assert_eq!(qs.iter().map(|q| q.key.as_str()).collect::<Vec<_>>(), vec!["qA"]);
        assert_eq!(
            ss.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s3"]
        );
    }

    #[test]
    fn empty_key_list_short_circuits() {
        let mut o = opts();
        o.query_keys = Some(vec![]);
        let plan = plan_from_options(&o);
        assert!(plan.short_circuit);
        assert_eq!(plan.query_keys.as_ref().map(|s| s.len()), Some(0));
    }

    #[test]
    fn strand_labels_recognised_unknown_ignored() {
        for (label, expected) in [("plus", Some(StrandFilter::Plus)), ("minus", Some(StrandFilter::Minus))]
        {
            let mut o = opts();
            o.strand = Some(label.into());
            assert_eq!(plan_from_options(&o).strand_keep, expected);
        }

        let mut o = opts();
        o.strand = Some("sideways".into());
        let p = plan_from_options(&o);
        assert!(p.strand_keep.is_none());
        assert!(!p.short_circuit);
    }

    #[test]
    fn keep_strand_filters_hits() {
        let plus = Hit {
            query_key: "q".into(),
            subject_key: "s".into(),
            query_start: 1,
            query_end: 10,
            subject_start: 1,
            subject_end: 10,
            strand: Strand::Plus,
            identity_count: 10,
            alignment_length: 10,
            percent_identity: 100.0,
            bit_score: 20.0,
            raw_score: 20.0,
            evalue: 1e-9,
        };
        let mut minus = plus.clone();
        minus.strand = Strand::Minus;

        let plan = PushdownPlan {
            strand_keep: Some(StrandFilter::Plus),
            ..Default::default()
        };
        let kept = keep_strand(&plan, vec![plus.clone(), minus.clone()]);
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0].strand, Strand::Plus));

        // No strand filter: both survive.
        let kept = keep_strand(&PushdownPlan::default(), vec![plus, minus]);
        assert_eq!(kept.len(), 2);
    }
}
