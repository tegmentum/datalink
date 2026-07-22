//! Smith-Waterman local alignment + Karlin-Altschul statistics.
//!
//! Two entry points because rust-bio's `Aligner<F>` requires the scoring
//! function type to be `Copy` — the natural way to parameterise scoring
//! (a `Box<dyn Fn>`) doesn't satisfy that. blastn takes match/mismatch
//! scalars and builds its own closure; blastp uses the `bio::scores::blosum62`
//! function pointer directly.

use bio::alignment::pairwise::Aligner;
use bio::alignment::{Alignment, AlignmentOperation};
use bio::scores::blosum62;

/// Raw alignment output before conversion to the WIT `hit` record. Positions
/// are 0-based half-open, matching rust-bio's convention; the caller
/// converts to 1-based inclusive at the WIT boundary.
pub struct RawAlignment {
    pub raw_score: i32,
    pub bit_score: f64,
    pub e_value: f64,
    pub identity_count: u32,
    pub alignment_length: u32,
    pub query_start_0: usize,
    pub query_end_0: usize,
    pub subject_start_0: usize,
    pub subject_end_0: usize,
}

/// BLASTN alignment. Reward / penalty are the per-base scores; the
/// scoring closure treats characters as case-insensitive and any `N` as a
/// mismatch (standard BLASTN behaviour).
pub fn run_blastn(
    query: &[u8],
    subject: &[u8],
    match_reward: i32,
    mismatch_penalty: i32,
    gap_open: i32,
    gap_extend: i32,
    k: f64,
    lambda: f64,
) -> Option<RawAlignment> {
    let scorer = move |a: u8, b: u8| {
        let au = a.to_ascii_uppercase();
        let bu = b.to_ascii_uppercase();
        if au == b'N' || bu == b'N' || au != bu {
            mismatch_penalty
        } else {
            match_reward
        }
    };
    let mut aligner =
        Aligner::with_capacity(query.len(), subject.len(), gap_open, gap_extend, scorer);
    let alignment = aligner.local(query, subject);
    finalise(alignment, k, lambda, query.len(), subject.len())
}

/// BLASTP alignment using rust-bio's tabulated BLOSUM62. `matrix` in the
/// WIT scoring variant is currently a no-op — the only implemented matrix
/// is BLOSUM62 and `scoring::resolve` rejects the others up front.
pub fn run_blastp(
    query: &[u8],
    subject: &[u8],
    gap_open: i32,
    gap_extend: i32,
    k: f64,
    lambda: f64,
) -> Option<RawAlignment> {
    let mut aligner =
        Aligner::with_capacity(query.len(), subject.len(), gap_open, gap_extend, blosum62);
    let alignment = aligner.local(query, subject);
    finalise(alignment, k, lambda, query.len(), subject.len())
}

/// Common post-processing: skip empty local paths, compute Karlin-Altschul
/// bit score and E-value, count identity.
fn finalise(
    alignment: Alignment,
    k: f64,
    lambda: f64,
    query_len: usize,
    subject_len: usize,
) -> Option<RawAlignment> {
    if alignment.operations.is_empty() || alignment.xend <= alignment.xstart {
        return None;
    }

    let raw_score = alignment.score;
    // Karlin-Altschul: S' = (lambda * S - ln K) / ln 2.
    let bit_score = (lambda * raw_score as f64 - k.ln()) / std::f64::consts::LN_2;
    // E = K * m * n * exp(-lambda * S). Effective lengths are approximated
    // by the raw sequence lengths — for pairwise (not database) search that
    // approximation is what NCBI's pairwise mode uses too.
    let e_value = k * query_len as f64 * subject_len as f64 * (-lambda * raw_score as f64).exp();

    let (identity_count, alignment_length) = count_identity(&alignment);

    Some(RawAlignment {
        raw_score,
        bit_score,
        e_value,
        identity_count,
        alignment_length,
        query_start_0: alignment.xstart,
        query_end_0: alignment.xend,
        subject_start_0: alignment.ystart,
        subject_end_0: alignment.yend,
    })
}

fn count_identity(alignment: &Alignment) -> (u32, u32) {
    let mut matches: u32 = 0;
    let mut length: u32 = 0;
    for op in &alignment.operations {
        match op {
            AlignmentOperation::Match => {
                matches += 1;
                length += 1;
            }
            AlignmentOperation::Subst
            | AlignmentOperation::Del
            | AlignmentOperation::Ins => {
                length += 1;
            }
            _ => {}
        }
    }
    (matches, length)
}
