//! Reverse-complement + coordinate translation for the minus-strand pass.
//!
//! `revcomp` intentionally does not depend on `bio::alphabets::dna` — the
//! four-plus-N alphabet we need is eight lines of code, and staying off
//! that module keeps the wasm binary smaller.

use crate::model::{Hit, Strand};

pub fn revcomp(seq: &[u8]) -> Vec<u8> {
    seq.iter().rev().map(|&b| complement(b)).collect()
}

fn complement(b: u8) -> u8 {
    match b {
        b'A' => b'T',
        b'a' => b't',
        b'T' => b'A',
        b't' => b'a',
        b'C' => b'G',
        b'c' => b'g',
        b'G' => b'C',
        b'g' => b'c',
        b'U' => b'A',
        b'u' => b'a',
        b'N' | b'n' => b,
        // Full IUPAC support is future work; unknown chars become N so the
        // BLASTN scoring closure treats them as mismatches consistently
        // whether they occur in the forward pass or the reverse.
        _ => b'N',
    }
}

/// Translate a hit produced against the reverse complement of a subject of
/// length `subject_len` back onto the original (forward) subject's
/// coordinates.
///
/// The hit's positions are 1-indexed inclusive (already converted by
/// `align_one`). For a hit at forward-strand-of-revcomp positions
/// `[s .. e]`, the same region on the original forward subject sits at
/// positions `[L-e+1 .. L-s+1]` — a mirror around the midpoint. `strand`
/// is forced to `minus` regardless of the input; if this function is
/// called at all, the caller was doing the minus-strand pass.
pub fn translate_minus_hit(mut hit: Hit, subject_len: u32) -> Hit {
    let s = hit.subject_start;
    let e = hit.subject_end;
    hit.subject_start = subject_len - e + 1;
    hit.subject_end = subject_len - s + 1;
    hit.strand = Strand::Minus;
    hit
}
