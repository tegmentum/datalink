//! Post-alignment filter + rank pass.
//!
//! Applied in order: E-value ceiling, identity floor, then per-query
//! best-N truncation. Ranking within a query is (bit-score desc, E-value
//! asc, subject-key asc) — the third key makes the output deterministic
//! across runs when two subjects tie on both scoring axes.

use crate::model::{Hit, SearchOptions};

pub fn apply(mut hits: Vec<Hit>, options: &SearchOptions) -> Vec<Hit> {
    if let Some(evalue_max) = options.evalue_max {
        hits.retain(|h| h.evalue <= evalue_max);
    }
    if let Some(min_identity) = options.min_identity {
        hits.retain(|h| h.percent_identity >= min_identity);
    }

    if let Some(max_targets) = options.max_target_seqs {
        rank_and_trim(&mut hits, max_targets as usize);
    }

    hits
}

fn rank_and_trim(hits: &mut Vec<Hit>, max_per_query: usize) {
    // Sort so hits for the same query_key are contiguous and internally
    // ordered by the ranking rule. total_cmp handles the NaN-worst case
    // without unwrap; it's a lexicographic comparison on the IEEE bit
    // representation but for the non-NaN, non-signed-zero values we
    // produce it matches partial_cmp exactly.
    hits.sort_unstable_by(|a, b| {
        a.query_key
            .cmp(&b.query_key)
            .then(b.bit_score.total_cmp(&a.bit_score))
            .then(a.evalue.total_cmp(&b.evalue))
            .then(a.subject_key.cmp(&b.subject_key))
    });

    let mut count = 0usize;
    let mut current: Option<String> = None;
    hits.retain(|h| {
        let same = current.as_deref() == Some(h.query_key.as_str());
        if !same {
            current = Some(h.query_key.clone());
            count = 0;
        }
        if count < max_per_query {
            count += 1;
            true
        } else {
            false
        }
    });
}
