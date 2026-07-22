//! Resolve the WIT `scoring` variant to concrete alignment parameters.
//!
//! Two responsibilities:
//! 1. Validate the caller's choice (unknown matrix, un-tabulated K/λ) and
//!    fail with `invalid-scoring` if we can't run it.
//! 2. Look up the Karlin-Altschul constants (K, λ) for the chosen scoring
//!    scheme — the alignment engine doesn't compute those, they're fitted
//!    constants published by NCBI.
//!
//! First-slice scope: only the two default schemes plus tunable variants
//! whose parameters happen to match those defaults are accepted. Extending
//! to further tabulated combinations is a straightforward addition to
//! `BLASTN_STATS` / `BLASTP_BLOSUM62_STATS`.

use crate::model::{
    BlastnScoring, BlastpScoring, Scoring,
};

/// Everything the aligner needs after scoring resolution: gap penalties
/// (score contributions, negative), the K/λ constants for its statistics,
/// whether to search both strands (BLASTN only), and which alignment
/// entry point to call.
pub struct Params {
    pub gap_open: i32,
    pub gap_extend: i32,
    pub k: f64,
    pub lambda: f64,
    pub both_strands: bool,
    pub kind: Kind,
}

pub enum Kind {
    Blastn {
        match_reward: i32,
        mismatch_penalty: i32,
    },
    Blastp,
}

/// BLASTN defaults. From NCBI blast_stat.c for reward=2, penalty=-3,
/// gap-open=5, gap-extend=2. Values are approximate to two decimals; the
/// bit-score / E-value scale matches NCBI blastn output within rounding.
const BLASTN_DEFAULT_REWARD: i32 = 2;
const BLASTN_DEFAULT_PENALTY: i32 = -3;
const BLASTN_DEFAULT_GAP_OPEN: i32 = -5;
const BLASTN_DEFAULT_GAP_EXTEND: i32 = -2;
const BLASTN_DEFAULT_K: f64 = 0.41;
const BLASTN_DEFAULT_LAMBDA: f64 = 0.625;

/// BLASTP defaults. BLOSUM62 with gap-open=11, gap-extend=1. K and λ
/// from the same table the scry-webfunctions-demo blastp component uses.
const BLASTP_DEFAULT_GAP_OPEN: i32 = -11;
const BLASTP_DEFAULT_GAP_EXTEND: i32 = -1;
const BLASTP_DEFAULT_K: f64 = 0.041;
const BLASTP_DEFAULT_LAMBDA: f64 = 0.267;

pub fn resolve(scoring: &Scoring) -> Result<Params, String> {
    match scoring {
        Scoring::BlastnDefault => Ok(blastn_default()),
        Scoring::Blastn(s) => resolve_blastn(s),
        Scoring::BlastpDefault => Ok(blastp_default()),
        Scoring::Blastp(s) => resolve_blastp(s),
    }
}

fn blastn_default() -> Params {
    Params {
        gap_open: BLASTN_DEFAULT_GAP_OPEN,
        gap_extend: BLASTN_DEFAULT_GAP_EXTEND,
        k: BLASTN_DEFAULT_K,
        lambda: BLASTN_DEFAULT_LAMBDA,
        both_strands: true,
        kind: Kind::Blastn {
            match_reward: BLASTN_DEFAULT_REWARD,
            mismatch_penalty: BLASTN_DEFAULT_PENALTY,
        },
    }
}

fn blastp_default() -> Params {
    Params {
        gap_open: BLASTP_DEFAULT_GAP_OPEN,
        gap_extend: BLASTP_DEFAULT_GAP_EXTEND,
        k: BLASTP_DEFAULT_K,
        lambda: BLASTP_DEFAULT_LAMBDA,
        both_strands: false,
        kind: Kind::Blastp,
    }
}

fn resolve_blastn(s: &BlastnScoring) -> Result<Params, String> {
    // First slice only accepts the default combination. Extending is a
    // matter of adding rows to a table of tabulated K/λ values.
    if s.match_reward != BLASTN_DEFAULT_REWARD
        || s.mismatch_penalty != BLASTN_DEFAULT_PENALTY
        || s.gap_open != BLASTN_DEFAULT_GAP_OPEN
        || s.gap_extend != BLASTN_DEFAULT_GAP_EXTEND
    {
        return Err(format!(
            "blastn scoring (reward={}, penalty={}, gap-open={}, gap-extend={}) \
             has no tabulated K/lambda; only (2, -3, -5, -2) is implemented",
            s.match_reward, s.mismatch_penalty, s.gap_open, s.gap_extend
        ));
    }
    Ok(Params {
        gap_open: s.gap_open,
        gap_extend: s.gap_extend,
        k: BLASTN_DEFAULT_K,
        lambda: BLASTN_DEFAULT_LAMBDA,
        both_strands: s.search_both_strands,
        kind: Kind::Blastn {
            match_reward: s.match_reward,
            mismatch_penalty: s.mismatch_penalty,
        },
    })
}

fn resolve_blastp(s: &BlastpScoring) -> Result<Params, String> {
    if !s.matrix.eq_ignore_ascii_case("BLOSUM62") {
        return Err(format!(
            "blastp matrix '{}' is not implemented; only 'BLOSUM62' is supported",
            s.matrix
        ));
    }
    if s.gap_open != BLASTP_DEFAULT_GAP_OPEN || s.gap_extend != BLASTP_DEFAULT_GAP_EXTEND {
        return Err(format!(
            "blastp BLOSUM62 with (gap-open={}, gap-extend={}) has no tabulated \
             K/lambda; only (-11, -1) is implemented",
            s.gap_open, s.gap_extend
        ));
    }
    Ok(Params {
        gap_open: s.gap_open,
        gap_extend: s.gap_extend,
        k: BLASTP_DEFAULT_K,
        lambda: BLASTP_DEFAULT_LAMBDA,
        both_strands: false,
        kind: Kind::Blastp,
    })
}
