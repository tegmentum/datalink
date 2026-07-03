//! Unit tests for the pure-Rust changeset codec. The invert/decode/count/
//! tables outputs are checked against **independently** hand-built golden
//! bytes/strings (not just serialize∘parse round-trips), so a bug in the
//! serializer can't hide behind a matching parser.

extern crate std;

use super::codec::{self, Change, Table, Val, OP_DELETE, OP_INSERT, OP_UPDATE};
use std::vec;
use std::vec::Vec;

// ---- tiny independent value encoders (NOT the codec's own) --------------

fn int_be(i: i64) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&i.to_be_bytes());
    v
}
fn float_be(f: f64) -> Vec<u8> {
    let mut v = vec![2u8];
    v.extend_from_slice(&f.to_bits().to_be_bytes());
    v
}
fn txt(s: &[u8]) -> Vec<u8> {
    // Only valid for len < 0x80 (single-byte varint) — fine for tests.
    assert!(s.len() < 0x80);
    let mut v = vec![3u8, s.len() as u8];
    v.extend_from_slice(s);
    v
}
fn undef() -> Vec<u8> {
    vec![0u8]
}
fn null_v() -> Vec<u8> {
    vec![5u8]
}

fn tv(s: &[u8]) -> Val {
    Val::Text(s.to_vec())
}

// A canonical 2-column table (col0 = PK) with one INSERT, one DELETE and one
// UPDATE (col1 changes "a" -> "b").
fn sample() -> Vec<Table> {
    vec![Table {
        n_col: 2,
        pk: vec![1, 0],
        name: b"t1".to_vec(),
        changes: vec![
            Change {
                op: OP_INSERT,
                indirect: 0,
                old: vec![],
                new: vec![Val::Int(5), tv(b"hi")],
            },
            Change {
                op: OP_DELETE,
                indirect: 0,
                old: vec![Val::Int(5), tv(b"hi")],
                new: vec![],
            },
            Change {
                op: OP_UPDATE,
                indirect: 0,
                old: vec![Val::Int(7), tv(b"a")],
                new: vec![Val::Undef, tv(b"b")],
            },
        ],
    }]
}

fn header_bytes() -> Vec<u8> {
    // 'T', varint(2), pk[1,0], "t1", nul.
    vec![0x54, 0x02, 0x01, 0x00, 0x74, 0x31, 0x00]
}

fn golden() -> Vec<u8> {
    let mut g = header_bytes();
    // INSERT
    g.extend_from_slice(&[0x12, 0x00]);
    g.extend(int_be(5));
    g.extend(txt(b"hi"));
    // DELETE
    g.extend_from_slice(&[0x09, 0x00]);
    g.extend(int_be(5));
    g.extend(txt(b"hi"));
    // UPDATE: old = int7, "a"; new = undef, "b"
    g.extend_from_slice(&[0x17, 0x00]);
    g.extend(int_be(7));
    g.extend(txt(b"a"));
    g.extend(undef());
    g.extend(txt(b"b"));
    g
}

#[test]
fn serialize_matches_hand_built_golden() {
    assert_eq!(codec::serialize(&sample()), golden());
}

#[test]
fn parse_then_serialize_roundtrips() {
    let g = golden();
    let parsed = codec::parse(&g).unwrap();
    assert_eq!(codec::serialize(&parsed), g);
    // Structure sanity.
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, b"t1");
    assert_eq!(parsed[0].pk, vec![1, 0]);
    assert_eq!(parsed[0].changes.len(), 3);
}

#[test]
fn invert_is_byte_exact() {
    // INSERT->DELETE (record = new), DELETE->INSERT (record = old),
    // UPDATE->UPDATE with the PK-aware swap.
    let mut expect = header_bytes();
    expect.extend_from_slice(&[0x09, 0x00]); // was INSERT
    expect.extend(int_be(5));
    expect.extend(txt(b"hi"));
    expect.extend_from_slice(&[0x12, 0x00]); // was DELETE
    expect.extend(int_be(5));
    expect.extend(txt(b"hi"));
    expect.extend_from_slice(&[0x17, 0x00]); // UPDATE
    // new old.*: PK col0 from old (int7), col1 from new ("b")
    expect.extend(int_be(7));
    expect.extend(txt(b"b"));
    // new new.*: PK col0 undefined, col1 from old ("a")
    expect.extend(undef());
    expect.extend(txt(b"a"));

    assert_eq!(codec::invert(&golden()).unwrap(), expect);
}

#[test]
fn invert_is_an_involution() {
    let g = golden();
    let once = codec::invert(&g).unwrap();
    let twice = codec::invert(&once).unwrap();
    assert_eq!(twice, g, "invert(invert(x)) must equal x");
}

#[test]
fn count_tables_decode() {
    let g = golden();
    assert_eq!(codec::count(&g).unwrap(), 3);
    assert_eq!(codec::tables_json(&g).unwrap(), r#"["t1"]"#);
    assert_eq!(
        codec::decode_json(&g).unwrap(),
        concat!(
            "[",
            r#"{"table":"t1","op":"INSERT","indirect":false,"new":[5,"hi"]}"#,
            ",",
            r#"{"table":"t1","op":"DELETE","indirect":false,"old":[5,"hi"]}"#,
            ",",
            r#"{"table":"t1","op":"UPDATE","indirect":false,"old":[7,"a"],"new":[null,"b"]}"#,
            "]"
        )
    );
}

#[test]
fn decode_distinguishes_null_from_undef_as_json_null() {
    // A DELETE whose col1 is a real SQL NULL (0x05) still renders as JSON null,
    // matching the embed path.
    let mut b = header_bytes();
    b.extend_from_slice(&[0x09, 0x00]);
    b.extend(int_be(1));
    b.extend(null_v());
    assert_eq!(
        codec::decode_json(&b).unwrap(),
        r#"[{"table":"t1","op":"DELETE","indirect":false,"old":[1,null]}]"#
    );
}

#[test]
fn float_roundtrips_and_decodes() {
    let mut b = header_bytes();
    b.extend_from_slice(&[0x12, 0x00]); // INSERT
    b.extend(int_be(1));
    b.extend(float_be(1.5));
    // Round-trip preserves the exact bits.
    assert_eq!(codec::serialize(&codec::parse(&b).unwrap()), b);
    assert_eq!(
        codec::decode_json(&b).unwrap(),
        r#"[{"table":"t1","op":"INSERT","indirect":false,"new":[1,1.5]}]"#
    );
}

// ---- concat --------------------------------------------------------------

fn one_table(changes: Vec<Change>) -> Vec<u8> {
    codec::serialize(&[Table {
        n_col: 2,
        pk: vec![1, 0],
        name: b"t1".to_vec(),
        changes,
    }])
}

fn ins(pk: i64, c1: &[u8]) -> Change {
    Change {
        op: OP_INSERT,
        indirect: 0,
        old: vec![],
        new: vec![Val::Int(pk), tv(c1)],
    }
}
fn del(pk: i64, c1: &[u8]) -> Change {
    Change {
        op: OP_DELETE,
        indirect: 0,
        old: vec![Val::Int(pk), tv(c1)],
        new: vec![],
    }
}
fn upd(pk: i64, old_c1: &[u8], new_c1: &[u8]) -> Change {
    Change {
        op: OP_UPDATE,
        indirect: 0,
        old: vec![Val::Int(pk), tv(old_c1)],
        new: vec![Val::Undef, tv(new_c1)],
    }
}

#[test]
fn concat_insert_then_update_folds_into_insert() {
    let a = one_table(vec![ins(5, b"x")]);
    let b = one_table(vec![upd(5, b"x", b"y")]);
    let r = codec::parse(&codec::concat(&a, &b).unwrap()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].changes.len(), 1);
    let ch = &r[0].changes[0];
    assert_eq!(ch.op, OP_INSERT);
    assert_eq!(ch.new, vec![Val::Int(5), tv(b"y")]);
}

#[test]
fn concat_delete_then_insert_becomes_update() {
    let a = one_table(vec![del(5, b"x")]);
    let b = one_table(vec![ins(5, b"z")]);
    let r = codec::parse(&codec::concat(&a, &b).unwrap()).unwrap();
    let ch = &r[0].changes[0];
    assert_eq!(ch.op, OP_UPDATE);
    assert_eq!(ch.old, vec![Val::Int(5), tv(b"x")]);
    // PK undefined in new.*, col1 = new value.
    assert_eq!(ch.new, vec![Val::Undef, tv(b"z")]);
}

#[test]
fn concat_update_then_update_spans_old_to_latest_new() {
    let a = one_table(vec![upd(5, b"a", b"b")]);
    let b = one_table(vec![upd(5, b"b", b"c")]);
    let r = codec::parse(&codec::concat(&a, &b).unwrap()).unwrap();
    let ch = &r[0].changes[0];
    assert_eq!(ch.op, OP_UPDATE);
    assert_eq!(ch.old, vec![Val::Int(5), tv(b"a")]); // earliest old
    assert_eq!(ch.new, vec![Val::Undef, tv(b"c")]); // latest new
}

#[test]
fn concat_insert_then_delete_vanishes() {
    let a = one_table(vec![ins(5, b"x")]);
    let b = one_table(vec![del(5, b"x")]);
    let out = codec::concat(&a, &b).unwrap();
    assert!(out.is_empty(), "row created then deleted => empty changeset");
    assert_eq!(codec::count(&out).unwrap(), 0);
}

#[test]
fn concat_update_then_delete_becomes_delete() {
    let a = one_table(vec![upd(5, b"a", b"b")]);
    let b = one_table(vec![del(5, b"b")]);
    let r = codec::parse(&codec::concat(&a, &b).unwrap()).unwrap();
    let ch = &r[0].changes[0];
    assert_eq!(ch.op, OP_DELETE);
    // old.* combines the delete's record with the update's original old value.
    assert_eq!(ch.old, vec![Val::Int(5), tv(b"a")]);
}

#[test]
fn concat_disjoint_rows_keeps_both() {
    let a = one_table(vec![ins(1, b"x")]);
    let b = one_table(vec![ins(2, b"y")]);
    let r = codec::parse(&codec::concat(&a, &b).unwrap()).unwrap();
    assert_eq!(r[0].changes.len(), 2);
}

#[test]
fn concat_update_then_update_that_reverts_is_dropped() {
    // "a"->"b" then "b"->"a" nets to no change => the row is dropped.
    let a = one_table(vec![upd(5, b"a", b"b")]);
    let b = one_table(vec![upd(5, b"b", b"a")]);
    let out = codec::concat(&a, &b).unwrap();
    assert!(out.is_empty());
}

// ---- robustness: never panic on malformed input --------------------------

#[test]
fn malformed_input_errors_not_panics() {
    let good = golden();
    // Every truncation prefix must return Err, never panic.
    for n in 0..good.len() {
        let slice = &good[..n];
        let _ = codec::invert(slice);
        let _ = codec::count(slice);
        let _ = codec::tables_json(slice);
        let _ = codec::decode_json(slice);
        let _ = codec::concat(slice, &good);
        let _ = codec::concat(&good, slice);
    }
    // Garbage tags.
    assert!(codec::count(&[0xFF, 0x00, 0x01]).is_err());
    assert!(codec::count(&[0x54]).is_err()); // lone table tag
}

#[test]
fn empty_changeset_is_well_defined() {
    assert_eq!(codec::count(&[]).unwrap(), 0);
    assert_eq!(codec::tables_json(&[]).unwrap(), "[]");
    assert_eq!(codec::decode_json(&[]).unwrap(), "[]");
    assert!(codec::invert(&[]).unwrap().is_empty());
}
