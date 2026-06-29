//! Correctness for the shared per-chunk scalar dispatch loop
//! ([`datalink_extcore::scalar_batch`]).
//!
//! The batching rewrite (resolve handle once, reuse one neutral scratch
//! buffer across rows, drop the per-row `Invokeinfo`) must be BEHAVIOUR-
//! identical to the old per-row loop. These tests pin that: same results,
//! same NULL propagation, identical error short-circuit.

use datalink_extcore::{scalar_batch, NeutralValue};

/// A stand-in for a contract `duckvalue` (a host value type). Mirrors the
/// closed neutral set so we can exercise marshalling both ways.
#[derive(Clone, Debug, PartialEq)]
enum Val {
    Null,
    Int64(i64),
    Text(String),
}

fn to_neutral(v: &Val) -> NeutralValue {
    match v {
        Val::Null => NeutralValue::Null,
        Val::Int64(n) => NeutralValue::Int64(*n),
        Val::Text(s) => NeutralValue::Text(s.clone()),
    }
}

fn from_neutral(v: NeutralValue) -> Val {
    match v {
        NeutralValue::Null => Val::Null,
        NeutralValue::Int64(n) => Val::Int64(n),
        NeutralValue::Text(s) => Val::Text(s),
        _ => Val::Null,
    }
}

/// `plus_one`: i64 -> i64. Errors on a negative input (to exercise the
/// error short-circuit). Sees NULL as 0 (the `Called` path) so we can tell
/// Propagate from Called apart.
fn dispatch(_idx: usize, args: &[NeutralValue]) -> Result<NeutralValue, String> {
    let n = match args.first() {
        Some(NeutralValue::Int64(n)) => *n,
        Some(NeutralValue::Null) => return Ok(NeutralValue::Int64(-999)), // Called marker
        _ => return Err("expected int".to_string()),
    };
    if n < 0 {
        return Err(format!("negative: {n}"));
    }
    Ok(NeutralValue::Int64(n + 1))
}

/// The OLD shape: re-marshal each row into a fresh Vec, no shared scratch.
/// Semantically what `call_scalar` per row did. Used as the oracle.
fn naive_per_row(
    propagate: bool,
    rows: Vec<Vec<Val>>,
) -> Result<Vec<Val>, String> {
    let mut out = Vec::with_capacity(rows.len());
    for args in rows.into_iter() {
        let neutral: Vec<NeutralValue> = args.iter().map(to_neutral).collect();
        if propagate && neutral.iter().any(|v| v.is_null()) {
            out.push(Val::Null);
            continue;
        }
        out.push(from_neutral(dispatch(0, &neutral)?));
    }
    Ok(out)
}

fn batched(propagate: bool, rows: Vec<Vec<Val>>) -> Result<Vec<Val>, String> {
    scalar_batch(
        0,
        propagate,
        rows,
        to_neutral,
        from_neutral,
        || Val::Null,
        dispatch,
        |e| e,
    )
}

fn sample_rows() -> Vec<Vec<Val>> {
    vec![
        vec![Val::Int64(1)],
        vec![Val::Int64(41)],
        vec![Val::Null],
        vec![Val::Int64(7)],
        vec![Val::Null],
    ]
}

#[test]
fn batched_matches_naive_propagate() {
    let rows = sample_rows();
    let got = batched(true, rows.clone()).unwrap();
    let oracle = naive_per_row(true, rows).unwrap();
    assert_eq!(got, oracle);
    // NULL rows propagate to NULL without invoking the core.
    assert_eq!(
        got,
        vec![
            Val::Int64(2),
            Val::Int64(42),
            Val::Null,
            Val::Int64(8),
            Val::Null
        ]
    );
}

#[test]
fn batched_matches_naive_called() {
    let rows = sample_rows();
    let got = batched(false, rows.clone()).unwrap();
    let oracle = naive_per_row(false, rows).unwrap();
    assert_eq!(got, oracle);
    // Called path: the core sees NULL and returns its marker (-999).
    assert_eq!(
        got,
        vec![
            Val::Int64(2),
            Val::Int64(42),
            Val::Int64(-999),
            Val::Int64(8),
            Val::Int64(-999)
        ]
    );
}

#[test]
fn error_short_circuits_like_per_row() {
    let rows = vec![vec![Val::Int64(1)], vec![Val::Int64(-5)], vec![Val::Int64(9)]];
    assert_eq!(
        batched(false, rows.clone()).unwrap_err(),
        naive_per_row(false, rows).unwrap_err()
    );
}

#[test]
fn empty_chunk_is_empty() {
    assert!(batched(true, Vec::new()).unwrap().is_empty());
}

#[test]
fn scratch_reuse_across_varied_arity_is_correct() {
    // Multi-arg rows (e.g. a 2-ary scalar): the reused scratch is cleared
    // each row, so a wider/narrower row never leaks stale cells.
    fn dispatch2(_i: usize, args: &[NeutralValue]) -> Result<NeutralValue, String> {
        Ok(NeutralValue::Int64(args.len() as i64))
    }
    let rows = vec![
        vec![Val::Int64(1), Val::Int64(2)],
        vec![Val::Int64(3), Val::Int64(4)],
    ];
    let out = scalar_batch(
        0,
        false,
        rows,
        to_neutral,
        from_neutral,
        || Val::Null,
        dispatch2,
        |e| e,
    )
    .unwrap();
    assert_eq!(out, vec![Val::Int64(2), Val::Int64(2)]);
}
