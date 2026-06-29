//! Before/after micro-benchmark for the guest-side scalar batching win.
//!
//!   cargo bench --bench scalar_batch
//!
//! Models one DuckDB DataChunk (2048 rows) through the generated
//! `call-scalar-batch` guest path, comparing the OLD shape (delegate to
//! `call_scalar` per row: a `Mutex` lock + `HashMap` handle lookup + a
//! fresh neutral `Vec` allocation EVERY row) against the NEW shared
//! [`datalink_extcore::scalar_batch`] loop (resolve once, reuse one
//! neutral scratch buffer). No external deps — `std::time::Instant`.

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Mutex;
use std::time::Instant;

use datalink_extcore::{scalar_batch, NeutralValue};

const ROWS: usize = 2048; // one STANDARD_VECTOR_SIZE chunk

#[derive(Clone)]
enum Val {
    Null,
    Int64(i64),
}

#[inline]
fn to_neutral(v: &Val) -> NeutralValue {
    match v {
        Val::Null => NeutralValue::Null,
        Val::Int64(n) => NeutralValue::Int64(*n),
    }
}

#[inline]
fn from_neutral(v: NeutralValue) -> Val {
    match v {
        NeutralValue::Int64(n) => Val::Int64(n),
        _ => Val::Null,
    }
}

#[inline]
fn dispatch(_idx: usize, args: &[NeutralValue]) -> Result<NeutralValue, String> {
    match args.first() {
        Some(NeutralValue::Int64(n)) => Ok(NeutralValue::Int64(n + 1)),
        _ => Ok(NeutralValue::Null),
    }
}

fn make_chunk() -> Vec<Vec<Val>> {
    (0..ROWS as i64).map(|i| vec![Val::Int64(i)]).collect()
}

/// OLD shape: `call_scalar_batch` delegated to `call_scalar` per row, which
/// re-resolved the handle (Mutex + HashMap) and allocated a neutral Vec.
fn old_per_row(table: &Mutex<HashMap<u32, usize>>, handle: u32, rows: Vec<Vec<Val>>) -> Vec<Val> {
    let mut out = Vec::with_capacity(rows.len());
    for args in rows.into_iter() {
        // re-resolve handle every row (the old delegation cost)
        let idx = *table.lock().unwrap().get(&handle).unwrap();
        // fresh per-row neutral allocation
        let neutral: Vec<NeutralValue> = args.iter().map(to_neutral).collect();
        out.push(from_neutral(dispatch(idx, &neutral).unwrap()));
    }
    out
}

fn new_batched(table: &Mutex<HashMap<u32, usize>>, handle: u32, rows: Vec<Vec<Val>>) -> Vec<Val> {
    let idx = *table.lock().unwrap().get(&handle).unwrap(); // resolve ONCE
    scalar_batch(idx, false, rows, to_neutral, from_neutral, || Val::Null, dispatch, |e| e).unwrap()
}

fn bench(name: &str, iters: u64, mut f: impl FnMut()) {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();
    let per_chunk = elapsed.as_nanos() as f64 / iters as f64;
    let per_row = per_chunk / ROWS as f64;
    println!("{name:<40} {per_chunk:>12.0} ns/chunk   {per_row:>8.2} ns/row   ({iters} iters)");
}

fn main() {
    println!("guest scalar batching: 2048-row chunk, before/after\n");
    let mut table = HashMap::new();
    table.insert(1u32, 0usize);
    let table = Mutex::new(table);
    let iters = 50_000u64;

    bench("OLD per-row (lock+lookup+alloc/row)", iters, || {
        let out = old_per_row(&table, 1, black_box(make_chunk()));
        black_box(out);
    });
    bench("NEW batched (resolve once + scratch)", iters, || {
        let out = new_batched(&table, 1, black_box(make_chunk()));
        black_box(out);
    });
}
